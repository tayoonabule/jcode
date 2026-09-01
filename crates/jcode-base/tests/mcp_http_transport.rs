//! End-to-end test of the Streamable HTTP MCP transport against a real local
//! server: a raw `tokio` TCP listener that speaks just enough HTTP to answer
//! `initialize`, `tools/list`, and `tools/call`.
//!
//! This exercises the real acceptance path (connect over HTTP, discover tools,
//! call one) rather than a mocked transport.

use jcode_base::mcp::{
    http::HttpTransport,
    oauth::{McpOAuthTokens, save_tokens},
    ContentBlock, McpClient, McpServerConfig,
};
use std::collections::HashMap;
use std::sync::Mutex;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

static ENV_LOCK: Mutex<()> = Mutex::new(());

fn http_config(url: String) -> McpServerConfig {
    McpServerConfig {
        command: String::new(),
        args: Vec::new(),
        env: HashMap::new(),
        shared: false,
        transport: Some("http".to_string()),
        url: Some(url),
        headers: HashMap::new(),
        oauth: None,
        enabled: None,
        disabled: None,
    }
}

/// Reply to one request. `sse` controls whether the body is a plain JSON
/// document or a `text/event-stream`, since servers may use either.
async fn serve(listener: tokio::net::TcpListener, sse: bool) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(async move {
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);

            let mut content_length = 0usize;
            let mut accept = String::new();
            let mut session = String::new();
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    break;
                }
                let lower = trimmed.to_ascii_lowercase();
                if let Some(value) = lower.strip_prefix("content-length:") {
                    content_length = value.trim().parse().unwrap_or(0);
                } else if let Some(value) = lower.strip_prefix("accept:") {
                    accept = value.trim().to_string();
                } else if let Some(value) = lower.strip_prefix("mcp-session-id:") {
                    session = value.trim().to_string();
                }
            }

            // The transport must advertise both response modes it can handle,
            // since that is how a server chooses between JSON and SSE.
            assert!(
                accept.contains("application/json") && accept.contains("text/event-stream"),
                "Accept header must offer both response modes, got {accept:?}"
            );

            let mut body = vec![0u8; content_length];
            if reader.read_exact(&mut body).await.is_err() {
                return;
            }
            let body = String::from_utf8_lossy(&body);
            let request: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let method = request["method"].as_str().unwrap_or_default();
            let id = request["id"].clone();

            // `initialize` establishes the session; every later request must
            // echo back the id the server assigned.
            if method != "initialize" {
                assert_eq!(
                    session, "test-session",
                    "requests after initialize must carry the assigned mcp-session-id"
                );
            }

            let result = match method {
                "initialize" => serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "serverInfo": {"name": "test-http-server", "version": "1"}
                }),
                "tools/list" => serde_json::json!({
                    "tools": [{
                        "name": "echo",
                        "description": "Echo the input",
                        "inputSchema": {"type": "object"}
                    }]
                }),
                "tools/call" => serde_json::json!({
                    "content": [{
                        "type": "text",
                        "text": request["params"]["arguments"]["text"]
                            .as_str()
                            .unwrap_or_default()
                    }]
                }),
                // Notifications carry no id and expect 202 with no body.
                _ => {
                    let _ = writer
                        .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n")
                        .await;
                    return;
                }
            };

            let payload =
                serde_json::json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
            let (content_type, payload) = if sse {
                (
                    "text/event-stream",
                    format!("event: message\ndata: {payload}\n\n"),
                )
            } else {
                ("application/json", payload)
            };

            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nmcp-session-id: test-session\r\nContent-Length: {}\r\n\r\n{payload}",
                payload.len()
            );
            let _ = writer.write_all(response.as_bytes()).await;
        });
    }
}

async fn run_case(sse: bool) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    tokio::spawn(serve(listener, sse));

    let client = McpClient::connect("http-test".to_string(), &http_config(url))
        .await
        .expect("connect over streamable http");

    assert_eq!(
        client.server_info().expect("server info").name,
        "test-http-server"
    );

    let tools = client.tools();
    assert_eq!(tools.len(), 1, "expected the one advertised tool");
    assert_eq!(tools[0].name, "echo");

    let result = client
        .call_tool("echo", serde_json::json!({"text": "hello over http"}))
        .await
        .expect("tool call");
    let text = match result.content.first().expect("tool returned content") {
        ContentBlock::Text { text } => text.as_str(),
        other => panic!("expected a text block, got {other:?}"),
    };
    assert_eq!(text, "hello over http");
}

#[tokio::test]
async fn streamable_http_transport_with_json_responses() {
    run_case(false).await;
}

#[tokio::test]
async fn streamable_http_transport_with_sse_responses() {
    run_case(true).await;
}

#[tokio::test]
async fn http_config_without_url_is_rejected() {
    let mut config = http_config(String::new());
    config.url = None;
    let error = match McpClient::connect("no-url".to_string(), &config).await {
        Ok(_) => panic!("must not connect without a url"),
        Err(error) => error,
    };
    assert!(
        format!("{error:#}").contains("url"),
        "error should mention the missing url: {error:#}"
    );
}

#[tokio::test]
async fn auth_like_forbidden_response_retries_with_stored_oauth_token() {
    let _env_lock = ENV_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let home = tempfile::tempdir().expect("home tempdir");
    let previous_home = std::env::var_os("JCODE_HOME");
    jcode_base::env::set_var("JCODE_HOME", home.path());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    tokio::spawn(async move {
        for attempt in 0..2 {
            let Ok((stream, _)) = listener.accept().await else {
                return;
            };
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut authorization = String::new();
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    break;
                }
                if trimmed.to_ascii_lowercase().starts_with("authorization:") {
                    authorization = trimmed["authorization:".len()..].trim().to_string();
                }
            }
            if attempt == 0 {
                let body = "Method doesn't allow unregistered callers without established identity";
                let response = format!(
                    "HTTP/1.1 403 Forbidden\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(), body
                );
                let _ = writer.write_all(response.as_bytes()).await;
            } else {
                assert_eq!(authorization, "Bearer stored-token");
                let body = r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(), body
                );
                let _ = writer.write_all(response.as_bytes()).await;
            }
        }
    });

    save_tokens(
        "forbidden-retry",
        &McpOAuthTokens {
            access_token: "stored-token".to_string(),
            refresh_token: None,
            expires_at: 0,
            client_id: None,
            token_endpoint: None,
            redirect_uri: None,
        },
    )
    .expect("persist test token");

    let config = http_config(url);
    let transport = HttpTransport::new("forbidden-retry".to_string(), &config)
        .expect("construct HTTP transport");
    let response = transport
        .send(r#"{"jsonrpc":"2.0","id":1,"method":"initialize"}"#, true)
        .await
        .expect("retry after auth-like 403");
    assert!(response.is_some(), "the retried request should return JSON");

    match previous_home {
        Some(value) => jcode_base::env::set_var("JCODE_HOME", value),
        None => jcode_base::env::remove_var("JCODE_HOME"),
    }
}
