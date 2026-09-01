//! Legacy MCP SSE transport.
//!
//! The original MCP HTTP transport opens one long-lived GET event stream. The
//! server first sends an `endpoint` event containing the URL to which JSON-RPC
//! messages must be POSTed. Responses and notifications then arrive on the
//! event stream.

use super::oauth::{self, McpOAuthTokens};
use super::protocol::{JsonRpcResponse, McpOAuthConfig, McpServerConfig};
use anyhow::{Context, Result};
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{Mutex, RwLock, oneshot};

const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

pub struct SseTransport {
    name: String,
    url: String,
    client: reqwest::Client,
    extra_headers: HashMap<String, String>,
    oauth_config: Option<McpOAuthConfig>,
    tokens: RwLock<Option<McpOAuthTokens>>,
    post_url: Arc<RwLock<Option<String>>>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<JsonRpcResponse>>>>,
    connect_lock: Mutex<()>,
    generation: Arc<AtomicU64>,
}

impl SseTransport {
    pub fn new(name: String, config: &McpServerConfig) -> Result<Self> {
        let url = config
            .url
            .clone()
            .context("SSE MCP server config has no `url`")?;
        Ok(Self {
            tokens: RwLock::new(oauth::load_tokens(&name)),
            name,
            url,
            client: super::http::shared_client(),
            extra_headers: config.headers.clone(),
            oauth_config: config.oauth.clone(),
            post_url: Arc::new(RwLock::new(None)),
            pending: Arc::new(Mutex::new(HashMap::new())),
            connect_lock: Mutex::new(()),
            generation: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Open the GET stream and discover the POST endpoint.
    pub async fn connect(&self) -> Result<()> {
        self.ensure_stream().await
    }

    async fn ensure_stream(&self) -> Result<()> {
        if self.post_url.read().await.is_some() {
            return Ok(());
        }

        let _guard = self.connect_lock.lock().await;
        if self.post_url.read().await.is_some() {
            return Ok(());
        }

        let mut authorized_retry = false;
        loop {
            let request = self
                .client
                .get(&self.url)
                .header(reqwest::header::ACCEPT, "text/event-stream")
                .header(reqwest::header::CACHE_CONTROL, "no-cache");
            let request = self.apply_headers(request).await;
            let response = request
                .send()
                .await
                .with_context(|| format!("MCP SSE request to '{}' failed", self.url))?;

            if response.status() == reqwest::StatusCode::UNAUTHORIZED && !authorized_retry {
                let challenge = response
                    .headers()
                    .get("www-authenticate")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                self.ensure_auth(challenge.as_deref()).await?;
                authorized_retry = true;
                continue;
            }
            if !response.status().is_success() {
                let status = response.status();
                let detail = response.text().await.unwrap_or_default();
                anyhow::bail!(
                    "MCP SSE server '{}' returned HTTP {}: {}",
                    self.name,
                    status.as_u16(),
                    detail.chars().take(200).collect::<String>()
                );
            }

            let mut stream = response.bytes_stream();
            let mut decoder = SseDecoder::default();
            let endpoint = 'endpoint: loop {
                let Some(Ok(chunk)) = stream.next().await else {
                    anyhow::bail!("MCP SSE server '{}' closed before sending an endpoint", self.name);
                };
                for event in decoder.push(&String::from_utf8_lossy(&chunk)) {
                    if let SseEvent::Endpoint(endpoint) = event {
                        break 'endpoint endpoint;
                    }
                }
            };
            let endpoint = url::Url::parse(&self.url)
                .and_then(|base| base.join(&endpoint))
                .context("MCP SSE server sent an invalid endpoint URL")?
                .to_string();

            let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
            *self.post_url.write().await = Some(endpoint);
            let post_url = Arc::clone(&self.post_url);
            let current_generation = Arc::clone(&self.generation);
            let pending = Arc::clone(&self.pending);
            tokio::spawn(async move {
                let mut stream = stream;
                let mut decoder = decoder;
                while let Some(Ok(chunk)) = stream.next().await {
                    for event in decoder.push(&String::from_utf8_lossy(&chunk)) {
                        if let SseEvent::Message(response) = event {
                            if let Some(id) = response.id {
                                if let Some(sender) = pending.lock().await.remove(&id) {
                                    let _ = sender.send(response);
                                }
                            }
                        }
                    }
                }
                for event in decoder.finish() {
                    if let SseEvent::Message(response) = event {
                        if let Some(id) = response.id {
                            if let Some(sender) = pending.lock().await.remove(&id) {
                                let _ = sender.send(response);
                            }
                        }
                    }
                }
                let mut current = post_url.write().await;
                // Do not clear a newer stream established after this one ended.
                if current_generation.load(Ordering::SeqCst) == generation {
                    *current = None;
                }
            });
            return Ok(());
        }
    }

    async fn apply_headers(&self, mut request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        for (key, value) in &self.extra_headers {
            request = request.header(key.as_str(), value.as_str());
        }
        if let Some(token) = self.tokens.read().await.as_ref() {
            request = request.header(
                reqwest::header::AUTHORIZATION,
                format!("Bearer {}", token.access_token),
            );
        }
        request
    }

    async fn clear_stream(&self) {
        *self.post_url.write().await = None;
    }

    async fn ensure_auth(&self, challenge: Option<&str>) -> Result<()> {
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
        if super::http::non_interactive() {
            anyhow::bail!(
                "MCP server '{}' requires OAuth sign-in; run `jcode` interactively to authorize",
                self.name
            );
        }
        let tokens = oauth::authorize(
            &self.name,
            &self.url,
            challenge,
            self.oauth_config.as_ref(),
            false,
        )
        .await?;
        *self.tokens.write().await = Some(tokens);
        Ok(())
    }

    pub async fn send(&self, body: &str, id: u64, expect_response: bool) -> Result<Option<JsonRpcResponse>> {
        let mut authorized_retry = false;
        loop {
            self.ensure_stream().await?;
            let endpoint = self
                .post_url
                .read()
                .await
                .clone()
                .context("MCP SSE stream has no message endpoint")?;

            let receiver = if expect_response {
                let (sender, receiver) = oneshot::channel();
                self.pending.lock().await.insert(id, sender);
                Some(receiver)
            } else {
                None
            };

            let request = self
                .client
                .post(endpoint)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header(reqwest::header::CONTENT_LENGTH, body.len())
                .header(reqwest::header::ACCEPT, "application/json, text/event-stream");
            let request = self.apply_headers(request).await;
            let response = request.body(body.to_string()).send().await?;

            if response.status() == reqwest::StatusCode::UNAUTHORIZED && !authorized_retry {
                self.pending.lock().await.remove(&id);
                let challenge = response
                    .headers()
                    .get("www-authenticate")
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_string);
                self.clear_stream().await;
                self.ensure_auth(challenge.as_deref()).await?;
                authorized_retry = true;
                continue;
            }
            if !response.status().is_success() && response.status() != reqwest::StatusCode::ACCEPTED {
                self.pending.lock().await.remove(&id);
                let status = response.status();
                let detail = response.text().await.unwrap_or_default();
                anyhow::bail!(
                    "MCP SSE server '{}' returned HTTP {}: {}",
                    self.name,
                    status.as_u16(),
                    detail.chars().take(200).collect::<String>()
                );
            }
            if !expect_response {
                return Ok(None);
            }

            if response.status() != reqwest::StatusCode::ACCEPTED {
                let content_type = response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if content_type.starts_with("application/json") {
                    let text = response.text().await?;
                    if let Ok(response) = serde_json::from_str::<JsonRpcResponse>(&text) {
                        self.pending.lock().await.remove(&id);
                        return Ok(Some(response));
                    }
                }
            }

            let response = tokio::time::timeout(REQUEST_TIMEOUT, receiver.unwrap())
                .await
                .context("MCP SSE request timeout")?
                .context("MCP SSE event stream closed")?;
            return Ok(Some(response));
        }
    }

    pub async fn notify(&self, body: &str) -> Result<()> {
        self.send(body, 0, false).await.map(|_| ())
    }

    pub async fn reauthenticate(&self) -> Result<()> {
        self.clear_stream().await;
        self.ensure_auth(None).await
    }
}

#[derive(Debug)]
enum SseEvent {
    Endpoint(String),
    Message(JsonRpcResponse),
}

#[derive(Default)]
struct SseDecoder {
    pending: String,
    event: String,
    data: String,
}

impl SseDecoder {
    fn push(&mut self, chunk: &str) -> Vec<SseEvent> {
        self.pending.push_str(chunk);
        let mut events = Vec::new();
        while let Some(newline) = self.pending.find('\n') {
            let line = self.pending[..newline].trim_end_matches('\r').to_string();
            self.pending.drain(..=newline);
            if line.is_empty() {
                if let Some(event) = self.take_event() {
                    events.push(event);
                }
            } else if let Some(value) = line.strip_prefix("event:") {
                self.event = value.trim().to_string();
            } else if let Some(value) = line.strip_prefix("data:") {
                if !self.data.is_empty() {
                    self.data.push('\n');
                }
                self.data.push_str(value.trim_start());
            }
        }
        events
    }

    fn finish(&mut self) -> Vec<SseEvent> {
        let pending = std::mem::take(&mut self.pending);
        if !pending.is_empty() {
            self.pending.push_str(&pending);
            self.pending.push('\n');
        }
        self.push("")
    }

    fn take_event(&mut self) -> Option<SseEvent> {
        if self.data.is_empty() {
            self.event.clear();
            return None;
        }
        let event = std::mem::take(&mut self.event);
        let data = std::mem::take(&mut self.data);
        if event == "endpoint" {
            Some(SseEvent::Endpoint(data))
        } else {
            serde_json::from_str(&data).ok().map(SseEvent::Message)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{SseDecoder, SseEvent};

    #[test]
    fn parses_endpoint_and_message_events_across_chunks() {
        let mut decoder = SseDecoder::default();
        assert!(decoder.push("event: endpoint\ndata: /message?sessionId=1\n\n").iter().any(
            |event| matches!(event, SseEvent::Endpoint(value) if value == "/message?sessionId=1")
        ));
        let mut events = Vec::new();
        for chunk in ["event: message\ndata: {\"jsonrpc\":\"2.0\",", "\"id\":4,\"result\":{}}\n\n"] {
            events.extend(decoder.push(chunk));
        }
        assert!(events.iter().any(|event| matches!(event, SseEvent::Message(response) if response.id == Some(4))));
    }
}
