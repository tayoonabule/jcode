//! End-to-end test for the legacy MCP SSE transport.

use jcode_base::mcp::{ContentBlock, McpClient, McpServerConfig};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, mpsc};

fn sse_config(url: String) -> McpServerConfig {
    McpServerConfig {
        command: String::new(),
        args: Vec::new(),
        env: HashMap::new(),
        shared: false,
        transport: Some("sse".to_string()),
        url: Some(url),
        headers: HashMap::new(),
        oauth: None,
        enabled: None,
        disabled: None,
    }
}

async fn serve(listener: tokio::net::TcpListener) {
    let (events_tx, mut events_rx) = mpsc::channel::<String>(8);
    let stream_writer: Arc<Mutex<Option<tokio::net::tcp::OwnedWriteHalf>>> =
        Arc::new(Mutex::new(None));
    let writer_slot = Arc::clone(&stream_writer);

    tokio::spawn(async move {
        while let Some(payload) = events_rx.recv().await {
            let mut writer = writer_slot.lock().await;
            if let Some(writer) = writer.as_mut() {
                let event = format!("event: message\ndata: {payload}\n\n");
                let _ = writer.write_all(event.as_bytes()).await;
            }
        }
    });

    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let events_tx = events_tx.clone();
        let stream_writer = Arc::clone(&stream_writer);
        tokio::spawn(async move {
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut request_line = String::new();
            if reader.read_line(&mut request_line).await.unwrap_or(0) == 0 {
                return;
            }
            let mut parts = request_line.split_whitespace();
            let method = parts.next().unwrap_or_default();
            let path = parts.next().unwrap_or_default();
            let mut content_length = 0usize;
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                if line.trim().is_empty() {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    content_length = value.trim().parse().unwrap_or(0);
                }
            }
            if method == "GET" {
                let header = concat!(
                    "HTTP/1.1 200 OK\r\n",
                    "Content-Type: text/event-stream\r\n",
                    "Cache-Control: no-cache\r\n",
                    "Connection: keep-alive\r\n\r\n",
                    "event: endpoint\n",
                    "data: /message?sessionId=test\n\n"
                );
                let _ = writer.write_all(header.as_bytes()).await;
                *stream_writer.lock().await = Some(writer);
                return;
            }
            let mut body = vec![0; content_length];
            if reader.read_exact(&mut body).await.is_err() {
                return;
            }
            let body = String::from_utf8_lossy(&body).to_string();
            let request: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            if request["method"] == "notifications/initialized" {
                let _ = writer
                    .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n")
                    .await;
                return;
            }
            let id = request["id"].clone();
            let result = match request["method"].as_str().unwrap_or_default() {
                "initialize" => serde_json::json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "serverInfo": {"name": "test-sse-server", "version": "1"}
                }),
                "tools/list" => serde_json::json!({
                    "tools": [{"name": "echo", "description": "Echo", "inputSchema": {"type": "object"}}]
                }),
                "tools/call" => serde_json::json!({
                    "content": [{"type": "text", "text": request["params"]["arguments"]["text"]}]
                }),
                _ => return,
            };
            if id.is_number() {
                let payload = serde_json::json!({
                    "jsonrpc": "2.0", "id": id, "result": result
                })
                .to_string();
                let _ = writer
                    .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n")
                    .await;
                let _ = events_tx.send(payload).await;
                return;
            }
            let _ = writer.write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n").await;
            let _ = path;
        });
    }
}

#[tokio::test]
async fn legacy_sse_transport_discovers_endpoint_and_calls_tool() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/sse", listener.local_addr().unwrap());
    tokio::spawn(serve(listener));

    let client = McpClient::connect("sse-test".to_string(), &sse_config(url))
        .await
        .expect("connect over legacy SSE");
    assert_eq!(client.server_info().unwrap().name, "test-sse-server");
    assert_eq!(client.tools().len(), 1);

    let result = client
        .call_tool("echo", serde_json::json!({"text": "hello over sse"}))
        .await
        .expect("tool call");
    match result.content.first().unwrap() {
        ContentBlock::Text { text } => assert_eq!(text, "hello over sse"),
        other => panic!("expected text result, got {other:?}"),
    }
}
