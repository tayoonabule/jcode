//! Streamable HTTP transport for remote MCP servers.
//!
//! One `POST /mcp` per JSON-RPC message, per the Streamable HTTP transport.
//! Responses arrive either as a single JSON body or as an SSE stream; both are
//! handled here.
//!
//! Memory notes: no persistent GET stream is held open, so an idle remote
//! server costs one `reqwest::Client` (connection pool) plus a session id and
//! token string. SSE responses are consumed incrementally and only the first
//! JSON-RPC response object is retained.

use super::oauth::{self, McpOAuthTokens};
use super::protocol::{JsonRpcResponse, McpOAuthConfig, McpServerConfig};
use anyhow::{Context, Result};
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

const SESSION_HEADER: &str = "mcp-session-id";
const INTERACTIVE_AUTH_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(10 * 60);

static AUTH_STARTS: OnceLock<std::sync::Mutex<HashMap<String, std::time::Instant>>> =
    OnceLock::new();

/// One interactive OAuth flow per server at a time. Multiple sessions can
/// discover the same expired remote server concurrently; without this gate
/// each request would open its own browser consent page before any of them had
/// a chance to persist the newly issued token.
async fn auth_flow_lock(name: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let locks = LOCKS.get_or_init(|| tokio::sync::Mutex::new(HashMap::new()));
    let mut guard = locks.lock().await;
    guard
        .entry(name.to_string())
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

fn interactive_auth_allowed(name: &str) -> bool {
    // The integration harness intentionally performs a second authorization
    // in one process to verify stale-token recovery.
    if std::env::var_os("JCODE_MCP_AUTH_AUTOFOLLOW").is_some() {
        return true;
    }
    let starts = AUTH_STARTS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    let mut starts = starts.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let now = std::time::Instant::now();
    if starts
        .get(name)
        .is_some_and(|started| now.duration_since(*started) < INTERACTIVE_AUTH_COOLDOWN)
    {
        return false;
    }

    // Jcode starts a fresh server process for each session, so an in-memory
    // cooldown alone still allows every new session to reopen the same stale
    // Google consent flow. Persist only the small timestamp, not credentials,
    // so the cooldown survives process restarts without changing token storage.
    let cooldown_path = interactive_auth_cooldown_path(name);
    if let Ok(value) = std::fs::read_to_string(&cooldown_path)
        && let Ok(started) = value.trim().parse::<u64>()
        && std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|now| now.as_secs().saturating_sub(started) < INTERACTIVE_AUTH_COOLDOWN.as_secs())
            .unwrap_or(false)
    {
        return false;
    }

    starts.insert(name.to_string(), now);
    if let Some(parent) = cooldown_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(epoch) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        let _ = std::fs::write(cooldown_path, epoch.as_secs().to_string());
    }
    true
}

fn interactive_auth_cooldown_path(name: &str) -> std::path::PathBuf {
    let safe: String = name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    if let Some(home) = std::env::var_os("JCODE_HOME") {
        std::path::PathBuf::from(home)
            .join("mcp-auth")
            .join(format!("{safe}.prompt"))
    } else {
        dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".jcode")
            .join("mcp-auth")
            .join(format!("{safe}.prompt"))
    }
}

/// The marker prevents concurrent sessions from opening duplicate browser
/// windows, but it must not survive the authorization attempt itself. If the
/// provider or browser flow fails, leaving it behind strands the next tool call
/// behind the ten-minute cooldown even though there are no usable credentials.
fn clear_interactive_auth_attempt(name: &str) {
    if let Some(starts) = AUTH_STARTS.get() {
        let mut starts = starts.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        starts.remove(name);
    }
    let _ = std::fs::remove_file(interactive_auth_cooldown_path(name));
}

/// One HTTP client shared by every remote MCP server.
///
/// A `reqwest::Client` owns a connection pool, DNS resolver and TLS config, so
/// building one per server cost ~200 KB each when measured. Sharing makes an
/// extra idle server nearly free; the pool still keys connections by host, so
/// servers do not interfere.
pub(crate) fn shared_client() -> reqwest::Client {
    static CLIENT: std::sync::OnceLock<reqwest::Client> = std::sync::OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .pool_max_idle_per_host(1)
                .build()
                .unwrap_or_default()
        })
        .clone()
}

pub struct HttpTransport {
    name: String,
    url: String,
    client: reqwest::Client,
    extra_headers: HashMap<String, String>,
    oauth_config: Option<McpOAuthConfig>,
    session_id: RwLock<Option<String>>,
    tokens: tokio::sync::RwLock<Option<McpOAuthTokens>>,
    /// Whether an interactive browser flow may be started for this server.
    interactive: bool,
}

impl HttpTransport {
    pub fn new(name: String, config: &McpServerConfig) -> Result<Self> {
        let url = config
            .url
            .clone()
            .context("HTTP MCP server config has no `url`")?;
        Ok(Self {
            tokens: tokio::sync::RwLock::new(oauth::load_tokens(&name)),
            name,
            url,
            client: shared_client(),
            extra_headers: config.headers.clone(),
            oauth_config: config.oauth.clone(),
            session_id: RwLock::new(None),
            interactive: !crate::mcp::http::non_interactive(),
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    fn session(&self) -> Option<String> {
        self.session_id
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }

    fn store_session(&self, headers: &reqwest::header::HeaderMap) {
        if let Some(value) = headers.get(SESSION_HEADER).and_then(|v| v.to_str().ok()) {
            let mut slot = self.session_id.write().unwrap_or_else(|p| p.into_inner());
            if slot.as_deref() != Some(value) {
                *slot = Some(value.to_string());
            }
        }
    }

    async fn bearer(&self) -> Option<String> {
        let tokens = self.tokens.read().await;
        tokens.as_ref().map(|t| t.access_token.clone())
    }

    /// Ensure a usable access token, refreshing or re-authorizing as needed.
    async fn ensure_auth(&self, challenge: Option<&str>) -> Result<()> {
        let flow_lock = auth_flow_lock(&self.name).await;
        let _flow_guard = flow_lock.lock().await;
        {
            let current = self.tokens.read().await.clone();
            if let Some(tokens) = current {
                if !tokens.is_expired() && challenge.is_none() {
                    return Ok(());
                }
                if let Some(refreshed) =
                    oauth::refresh(&self.name, &tokens, self.oauth_config.as_ref()).await
                {
                    *self.tokens.write().await = Some(refreshed);
                    return Ok(());
                }
            }
        }

        if !self.interactive {
            anyhow::bail!(
                "MCP server '{}' requires OAuth sign-in; run `jcode` interactively to authorize",
                self.name
            );
        }

        if !interactive_auth_allowed(&self.name) {
            anyhow::bail!(
                "MCP server '{}' recently opened an OAuth sign-in window; refusing to open another for 10 minutes",
                self.name
            );
        }

        let auth_result = oauth::authorize(
            &self.name,
            &self.url,
            challenge,
            self.oauth_config.as_ref(),
            false,
        )
        .await;
        clear_interactive_auth_attempt(&self.name);
        let tokens = auth_result?;
        *self.tokens.write().await = Some(tokens);
        Ok(())
    }

    /// Some first-party MCP gateways return HTTP 200 with an `isError` tool
    /// result instead of a 401 challenge. Allow the client layer to retry those
    /// calls through the same refresh/browser flow without exposing internals.
    pub async fn reauthenticate(&self) -> Result<()> {
        self.ensure_auth(None).await
    }

    /// Send one JSON-RPC message. `expect_response` is false for notifications,
    /// where servers reply `202 Accepted` with no body.
    pub async fn send(&self, body: &str, expect_response: bool) -> Result<Option<JsonRpcResponse>> {
        let mut authorized_retry = false;
        loop {
            let mut req = self
                .client
                .post(&self.url)
                .header("content-type", "application/json")
                .header("accept", "application/json, text/event-stream");
            if let Some(session) = self.session() {
                req = req.header(SESSION_HEADER, session);
            }
            if let Some(token) = self.bearer().await {
                req = req.header("authorization", format!("Bearer {token}"));
            }
            for (key, value) in &self.extra_headers {
                req = req.header(key.as_str(), value.as_str());
            }

            let resp = req
                .body(body.to_string())
                .send()
                .await
                .with_context(|| format!("MCP HTTP request to '{}' failed", self.url))?;

            let status = resp.status();
            if status == reqwest::StatusCode::UNAUTHORIZED && !authorized_retry {
                let challenge = resp
                    .headers()
                    .get("www-authenticate")
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string);
                self.ensure_auth(challenge.as_deref()).await?;
                authorized_retry = true;
                continue;
            }
            if status == reqwest::StatusCode::FORBIDDEN && !authorized_retry {
                let detail = resp.text().await.unwrap_or_default();
                if is_auth_error_text(&detail) {
                    self.ensure_auth(None).await?;
                    authorized_retry = true;
                    continue;
                }
                anyhow::bail!(
                    "MCP server '{}' returned HTTP {}: {}",
                    self.name,
                    status.as_u16(),
                    detail.chars().take(200).collect::<String>()
                );
            }
            if !status.is_success() {
                let detail = resp.text().await.unwrap_or_default();
                anyhow::bail!(
                    "MCP server '{}' returned HTTP {}: {}",
                    self.name,
                    status.as_u16(),
                    detail.chars().take(200).collect::<String>()
                );
            }

            self.store_session(resp.headers());

            if !expect_response || status == reqwest::StatusCode::ACCEPTED {
                return Ok(None);
            }

            let is_sse = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .is_some_and(|v| v.starts_with("text/event-stream"));

            if !is_sse {
                let text = resp.text().await?;
                return Ok(serde_json::from_str::<JsonRpcResponse>(&text).ok());
            }

            return Ok(read_sse_response(resp).await);
        }
    }

    /// Explicitly start the browser sign-in flow (used by `mcp login`).
    pub async fn login(&self) -> Result<()> {
        let auth_result = oauth::authorize(
            &self.name,
            &self.url,
            None,
            self.oauth_config.as_ref(),
            false,
        )
        .await;
        clear_interactive_auth_attempt(&self.name);
        let tokens = auth_result?;
        *self.tokens.write().await = Some(tokens);
        Ok(())
    }
}

pub(crate) fn is_auth_error_text(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("missing required authentication")
        || text.contains("expected oauth 2 access token")
        || text.contains("authentication credential")
        || text.contains("unregistered caller")
        || text.contains("without established identity")
}

/// Incremental SSE decoder.
///
/// Holds at most one partial line plus one event's `data:` payload, so peak
/// memory is bounded by a single event rather than the whole stream.
#[derive(Default)]
struct SseDecoder {
    pending: String,
    data: String,
}

impl SseDecoder {
    /// Feed a chunk; returns the first complete JSON-RPC response it yields.
    fn push(&mut self, chunk: &str) -> Option<JsonRpcResponse> {
        self.pending.push_str(chunk);
        while let Some(newline) = self.pending.find('\n') {
            let line: String = self.pending[..newline].trim_end_matches('\r').to_string();
            self.pending.drain(..=newline);

            if line.is_empty() {
                if let Some(parsed) = self.take_event() {
                    return Some(parsed);
                }
                continue;
            }
            if let Some(rest) = line.strip_prefix("data:") {
                self.data.push_str(rest.trim_start());
            }
        }
        None
    }

    /// Flush a final event that arrived without a trailing blank line.
    fn finish(&mut self) -> Option<JsonRpcResponse> {
        let pending = std::mem::take(&mut self.pending);
        if let Some(rest) = pending.trim_end().strip_prefix("data:") {
            self.data.push_str(rest.trim_start());
        }
        self.take_event()
    }

    fn take_event(&mut self) -> Option<JsonRpcResponse> {
        if self.data.is_empty() {
            return None;
        }
        let data = std::mem::take(&mut self.data);
        // Streamable HTTP servers may emit JSON-RPC notifications such as
        // `notifications/progress` before the response to the request. They
        // deserialize into this type with no id/result, but are not the tool
        // response. Keep scanning the SSE stream until an actual response
        // arrives instead of reporting "No result from tool call".
        serde_json::from_str::<JsonRpcResponse>(&data)
            .ok()
            .filter(|response| response.id.is_some())
    }
}

/// Read an SSE body and return the first JSON-RPC response found.
async fn read_sse_response(resp: reqwest::Response) -> Option<JsonRpcResponse> {
    let mut stream = resp.bytes_stream();
    let mut decoder = SseDecoder::default();

    while let Some(Ok(chunk)) = stream.next().await {
        if let Some(parsed) = decoder.push(&String::from_utf8_lossy(&chunk)) {
            return Some(parsed);
        }
    }

    decoder.finish()
}

/// Browser flows are impossible in headless/daemon-only runs.
pub fn non_interactive() -> bool {
    std::env::var("JCODE_MCP_NO_BROWSER").is_ok_and(|v| v != "0")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode a body one byte at a time, which also proves the decoder handles
    /// events split across chunk boundaries.
    fn sse(body: &str) -> Option<JsonRpcResponse> {
        let mut decoder = SseDecoder::default();
        for ch in body.chars() {
            if let Some(parsed) = decoder.push(&ch.to_string()) {
                return Some(parsed);
            }
        }
        decoder.finish()
    }

    #[test]
    fn parses_json_rpc_from_sse_event() {
        let parsed = sse("event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n")
            .expect("response");
        assert_eq!(parsed.id, Some(1));
        assert!(parsed.result.is_some());
    }

    #[test]
    fn ignores_comments_and_unterminated_noise() {
        assert!(sse(": keepalive\n\n").is_none());
    }

    #[test]
    fn accepts_final_event_without_trailing_blank_line() {
        let parsed =
            sse("data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{}}").expect("trailing event");
        assert_eq!(parsed.id, Some(7));
    }

    #[test]
    fn skips_events_that_are_not_json_rpc() {
        let parsed = sse("data: not json\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{}}\n\n")
            .expect("second event");
        assert_eq!(parsed.id, Some(3));
    }

    #[test]
    fn skips_json_rpc_notifications_before_response() {
        let parsed = sse(
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{}}\n\n\
             data: {\"jsonrpc\":\"2.0\",\"id\":9,\"result\":{}}\n\n",
        )
        .expect("response after notification");
        assert_eq!(parsed.id, Some(9));
    }
}
