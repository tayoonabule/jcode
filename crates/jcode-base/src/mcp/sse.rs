//! Legacy MCP SSE transport.
//!
//! The original MCP HTTP transport opens one long-lived GET event stream. The
//! server first sends an `endpoint` event containing the URL to which JSON-RPC
//! messages must be POSTed. Responses and notifications then arrive on the
//! event stream.

use super::oauth::{self, McpOAuthTokens};
use super::pending::{self, PendingMap};
use super::protocol::{JsonRpcResponse, McpOAuthConfig, McpServerConfig};
use anyhow::{Context, Result};
use futures::StreamExt;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{Mutex, RwLock};

pub struct SseTransport {
    name: String,
    url: String,
    client: reqwest::Client,
    extra_headers: HashMap<String, String>,
    oauth_config: Option<McpOAuthConfig>,
    tokens: RwLock<Option<McpOAuthTokens>>,
    post_url: Arc<RwLock<Option<String>>>,
    pending: PendingMap,
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
            pending: pending::new_pending(),
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
                    anyhow::bail!(
                        "MCP SSE server '{}' closed before sending an endpoint",
                        self.name
                    );
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
                            pending::resolve(&pending, response).await;
                        }
                    }
                }
                for event in decoder.finish() {
                    if let SseEvent::Message(response) = event {
                        pending::resolve(&pending, response).await;
                    }
                }
                // The stream is the only thing that could answer the requests
                // still in flight, so wake their callers rather than leaving
                // each to sit out its full reply deadline.
                pending::fail_all(&pending).await;
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
        // Shares the HTTP transport's flow lock and cooldown, so a server
        // reachable over both transports cannot open two browser windows.
        super::http::ensure_oauth(
            &self.name,
            &self.url,
            challenge,
            self.oauth_config.as_ref(),
            &self.tokens,
            !super::http::non_interactive(),
        )
        .await
    }

    pub async fn send(
        &self,
        body: &str,
        id: u64,
        expect_response: bool,
    ) -> Result<Option<JsonRpcResponse>> {
        let mut authorized_retry = false;
        loop {
            self.ensure_stream().await?;
            let endpoint = self
                .post_url
                .read()
                .await
                .clone()
                .context("MCP SSE stream has no message endpoint")?;

            // Register before sending: the reply travels on a separate stream
            // and can land before the POST returns.
            let waiter = if expect_response {
                Some(pending::PendingRequest::register(&self.pending, id).await)
            } else {
                None
            };

            let request = self
                .client
                .post(endpoint)
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header(reqwest::header::CONTENT_LENGTH, body.len())
                .header(
                    reqwest::header::ACCEPT,
                    "application/json, text/event-stream",
                );
            let request = self.apply_headers(request).await;
            let response = match request.body(body.to_string()).send().await {
                Ok(response) => response,
                Err(error) => {
                    // Release the slot before propagating, so a failed POST
                    // does not leave a waiter the stream will never answer.
                    if let Some(waiter) = waiter {
                        waiter.cancel().await;
                    }
                    return Err(error)
                        .with_context(|| format!("MCP SSE request to '{}' failed", self.name));
                }
            };
            let status = response.status();

            if status == reqwest::StatusCode::UNAUTHORIZED && !authorized_retry {
                if let Some(waiter) = waiter {
                    waiter.cancel().await;
                }
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
            if !status.is_success() {
                if let Some(waiter) = waiter {
                    waiter.cancel().await;
                }
                let detail = response.text().await.unwrap_or_default();
                anyhow::bail!(
                    "MCP SSE server '{}' returned HTTP {}: {}",
                    self.name,
                    status.as_u16(),
                    detail.chars().take(200).collect::<String>()
                );
            }
            let Some(waiter) = waiter else {
                return Ok(None);
            };

            // Most servers answer `202 Accepted` and deliver the response on
            // the event stream, but some reply inline with JSON.
            if status != reqwest::StatusCode::ACCEPTED {
                let content_type = response
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok())
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                if content_type.starts_with("application/json")
                    && let Ok(text) = response.text().await
                    && let Ok(inline) = serde_json::from_str::<JsonRpcResponse>(&text)
                {
                    waiter.cancel().await;
                    return Ok(Some(inline));
                }
            }

            // The reply deadline is enforced once by `McpHandle::request` so the
            // server's configured `timeout_secs` applies to every transport.
            return waiter
                .recv()
                .await
                .context("MCP SSE event stream closed")
                .map(Some);
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

/// Framing comes from the shared decoder; this only assigns meaning to the
/// `endpoint` and message events the legacy transport cares about.
#[derive(Default)]
struct SseDecoder {
    inner: super::sse_wire::SseDecoder,
}

impl SseDecoder {
    fn push(&mut self, chunk: &str) -> Vec<SseEvent> {
        self.inner
            .push(chunk)
            .into_iter()
            .filter_map(classify)
            .collect()
    }

    fn finish(&mut self) -> Vec<SseEvent> {
        self.inner.finish().and_then(classify).into_iter().collect()
    }
}

fn classify(event: super::sse_wire::RawEvent) -> Option<SseEvent> {
    if event.name == "endpoint" {
        Some(SseEvent::Endpoint(event.data))
    } else {
        serde_json::from_str(&event.data)
            .ok()
            .map(SseEvent::Message)
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
        for chunk in [
            "event: message\ndata: {\"jsonrpc\":\"2.0\",",
            "\"id\":4,\"result\":{}}\n\n",
        ] {
            events.extend(decoder.push(chunk));
        }
        assert!(
            events.iter().any(
                |event| matches!(event, SseEvent::Message(response) if response.id == Some(4))
            )
        );
    }

    /// A request cancelled before its reply (a re-authorized retry, say) must
    /// release its slot immediately, not on a detached task that could race a
    /// re-registration of the same id and delete the retry's waiter.
    #[tokio::test]
    async fn cancelling_a_request_frees_the_id_for_an_immediate_retry() {
        use super::pending::{self, PendingRequest};

        let pending = pending::new_pending();
        let first = PendingRequest::register(&pending, 7).await;
        first.cancel().await;
        assert!(
            pending.lock().await.is_empty(),
            "cancel must take effect before the retry re-registers"
        );

        // The retry reuses the same JSON-RPC id, as the transport does.
        let retry = PendingRequest::register(&pending, 7).await;
        pending::resolve(
            &pending,
            serde_json::from_value(serde_json::json!({
                "jsonrpc": "2.0", "id": 7, "result": {}
            }))
            .expect("response"),
        )
        .await;
        assert_eq!(
            retry.recv().await.expect("the retry must be answered").id,
            Some(7)
        );
    }
}
