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

/// Servers currently waiting on a human to complete a browser sign-in.
///
/// A sign-in is triggered from inside a request, but a person takes far longer
/// than any sane reply timeout, so the caller's deadline must not count that
/// wait against the server. `McpHandle::request` consults this instead of
/// giving every remote request a timeout long enough to cover a human.
static AUTH_IN_PROGRESS: OnceLock<std::sync::Mutex<HashMap<String, usize>>> = OnceLock::new();

fn auth_in_progress() -> &'static std::sync::Mutex<HashMap<String, usize>> {
    AUTH_IN_PROGRESS.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Whether a browser sign-in for `name` is waiting on the user right now.
pub(crate) fn interactive_auth_in_progress(name: &str) -> bool {
    auth_in_progress()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains_key(name)
}

/// Marks a server as waiting on a human for as long as it is held.
///
/// Counted rather than boolean because one server can be reached over both
/// remote transports, and an inner flow must not clear an outer one's mark.
struct InteractiveAuthInProgress<'a> {
    name: &'a str,
}

impl<'a> InteractiveAuthInProgress<'a> {
    fn begin(name: &'a str) -> Self {
        *auth_in_progress()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .entry(name.to_string())
            .or_insert(0) += 1;
        Self { name }
    }
}

impl Drop for InteractiveAuthInProgress<'_> {
    fn drop(&mut self) {
        let mut in_progress = auth_in_progress()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = in_progress.get_mut(self.name) {
            *count -= 1;
            if *count == 0 {
                in_progress.remove(self.name);
            }
        }
    }
}

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
    let mut starts = starts
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        let mut starts = starts
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
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
        let tokens = oauth::load_tokens(&name);
        // Older builds left the prompt marker behind after a successful flow.
        // If credentials are already persisted, that marker cannot represent
        // the only active authorization attempt and should not block recovery
        // after an expired-token refresh fails.
        if tokens.is_some() {
            clear_interactive_auth_attempt(&name);
        }
        Ok(Self {
            tokens: tokio::sync::RwLock::new(tokens),
            name,
            url,
            client: shared_client(),
            extra_headers: config.headers.clone(),
            oauth_config: config.oauth.clone(),
            session_id: RwLock::new(None),
            interactive: !crate::mcp::http::non_interactive(),
        })
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
        ensure_oauth(
            &self.name,
            &self.url,
            challenge,
            self.oauth_config.as_ref(),
            &self.tokens,
            self.interactive,
        )
        .await
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
            if !status.is_success() {
                let detail = resp.text().await.unwrap_or_default();
                // Some gateways answer an expired credential with 403 and an
                // explanatory body rather than a 401 challenge, so treat that
                // body as the challenge and retry once.
                if status == reqwest::StatusCode::FORBIDDEN
                    && !authorized_retry
                    && is_auth_error_text(&detail)
                {
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
}

pub(crate) fn is_auth_error_text(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("missing required authentication")
        || text.contains("expected oauth 2 access token")
        || text.contains("authentication credential")
        || text.contains("unregistered caller")
        || text.contains("without established identity")
}

/// Read an SSE body and return the first JSON-RPC response found.
async fn read_sse_response(resp: reqwest::Response) -> Option<JsonRpcResponse> {
    let mut stream = resp.bytes_stream();
    let mut decoder = super::sse_wire::SseDecoder::default();

    while let Some(Ok(chunk)) = stream.next().await {
        for event in decoder.push(&String::from_utf8_lossy(&chunk)) {
            if let Some(response) = json_rpc_response(&event.data) {
                return Some(response);
            }
        }
    }

    decoder
        .finish()
        .and_then(|event| json_rpc_response(&event.data))
}

/// Parse an SSE payload as the response to our request.
///
/// Streamable HTTP servers may emit JSON-RPC notifications such as
/// `notifications/progress` before the response. Those deserialize into the
/// same type with no id, so require an id rather than reporting "No result
/// from tool call" for the first notification that arrives.
fn json_rpc_response(data: &str) -> Option<JsonRpcResponse> {
    serde_json::from_str::<JsonRpcResponse>(data)
        .ok()
        .filter(|response| response.id.is_some())
}

/// Browser flows are impossible in headless/daemon-only runs.
pub fn non_interactive() -> bool {
    std::env::var("JCODE_MCP_NO_BROWSER").is_ok_and(|v| v != "0")
}

/// Ensure a remote MCP server has usable OAuth credentials.
///
/// Shared by both remote transports so they cannot drift apart on the parts
/// that matter for user experience: the per-server flow lock and the reload
/// from disk that stop concurrent connections from each opening their own
/// browser window, and the cooldown that stops a failing server from
/// reopening one repeatedly.
pub(crate) async fn ensure_oauth(
    name: &str,
    url: &str,
    challenge: Option<&str>,
    oauth_config: Option<&McpOAuthConfig>,
    tokens: &tokio::sync::RwLock<Option<McpOAuthTokens>>,
    interactive: bool,
) -> Result<()> {
    let flow_lock = auth_flow_lock(name).await;
    let _flow_guard = flow_lock.lock().await;
    // Another transport may have completed the interactive flow while we were
    // waiting for this server's lock, so refresh the in-memory view from disk
    // before deciding that authorization is still required.
    if let Some(persisted) = oauth::load_tokens(name) {
        *tokens.write().await = Some(persisted);
    }
    {
        let current = tokens.read().await.clone();
        if let Some(current) = current {
            if !current.is_expired() && challenge.is_none() {
                return Ok(());
            }
            if let Some(refreshed) = oauth::refresh(name, &current, oauth_config).await {
                *tokens.write().await = Some(refreshed);
                return Ok(());
            }
        }
    }

    if !interactive {
        anyhow::bail!(
            "MCP server '{name}' requires OAuth sign-in; run `jcode` interactively to authorize"
        );
    }

    if !interactive_auth_allowed(name) {
        anyhow::bail!(
            "MCP server '{name}' recently opened an OAuth sign-in window; refusing to open another for 10 minutes"
        );
    }

    // The caller's reply deadline can cancel this future mid-flow. Clear the
    // marker from a guard so an interrupted sign-in does not strand the next
    // attempt behind the cooldown with no usable credentials.
    let attempt = AuthAttempt { name };
    // Signing in waits on a person, so tell the caller's deadline to stop
    // counting while the browser flow is open.
    let waiting = InteractiveAuthInProgress::begin(name);
    let auth_result = oauth::authorize(name, url, challenge, oauth_config, false).await;
    drop(waiting);
    drop(attempt);
    *tokens.write().await = Some(auth_result?);
    Ok(())
}

/// Clears a server's interactive-auth marker when the attempt ends, including
/// when the future is dropped before the flow returns.
struct AuthAttempt<'a> {
    name: &'a str,
}

impl Drop for AuthAttempt<'_> {
    fn drop(&mut self) {
        clear_interactive_auth_attempt(self.name);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A browser sign-in waits on a person, so the request that triggered it
    /// must not be timed out while it is open. Before this, the first call to
    /// an unauthorized remote server failed after the 30s default reply
    /// deadline no matter how fast the server itself answered.
    #[test]
    fn an_open_sign_in_suspends_the_callers_reply_deadline() {
        assert!(!interactive_auth_in_progress("sign-in-test"));
        {
            let _waiting = InteractiveAuthInProgress::begin("sign-in-test");
            assert!(interactive_auth_in_progress("sign-in-test"));
            {
                // The same server reached over both transports must not have
                // the inner flow clear the outer one's mark.
                let _nested = InteractiveAuthInProgress::begin("sign-in-test");
            }
            assert!(
                interactive_auth_in_progress("sign-in-test"),
                "a nested flow ending must not end the outer one"
            );
        }
        assert!(
            !interactive_auth_in_progress("sign-in-test"),
            "the mark must be cleared once no flow is open"
        );
    }

    /// Decode a body one byte at a time, which also proves the decoder handles
    /// events split across chunk boundaries.
    fn sse(body: &str) -> Option<JsonRpcResponse> {
        let mut decoder = super::super::sse_wire::SseDecoder::default();
        for ch in body.chars() {
            for event in decoder.push(&ch.to_string()) {
                if let Some(parsed) = json_rpc_response(&event.data) {
                    return Some(parsed);
                }
            }
        }
        decoder
            .finish()
            .and_then(|event| json_rpc_response(&event.data))
    }

    #[test]
    fn parses_json_rpc_from_sse_event() {
        let parsed = sse(
            "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"ok\":true}}\n\n",
        )
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
        let parsed =
            sse("data: not json\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{}}\n\n")
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
