//! Measures the resident-memory cost of idle remote (HTTP) MCP connections.
//!
//! The design claim is that an idle remote server is cheap because no SSE
//! stream is held open: the cost is one pooled connection plus a session id and
//! token string. This measures it instead of asserting it.
//!
//! Ignored by default because RSS is noisy and platform-specific. Run with:
//!
//! ```sh
//! cargo test -p jcode-base --profile selfdev --test mcp_http_memory -- --ignored --nocapture
//! ```

use jcode_base::mcp::{McpClient, McpServerConfig};
use std::collections::HashMap;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const SERVERS: usize = 50;

/// Resident set size in kilobytes for the current process.
fn rss_kb() -> u64 {
    #[cfg(target_os = "macos")]
    {
        let out = std::process::Command::new("ps")
            .args(["-o", "rss=", "-p"])
            .arg(std::process::id().to_string())
            .output()
            .expect("ps");
        String::from_utf8_lossy(&out.stdout)
            .trim()
            .parse()
            .unwrap_or(0)
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::fs::read_to_string("/proc/self/statm")
            .ok()
            .and_then(|s| s.split_whitespace().nth(1).and_then(|v| v.parse::<u64>().ok()))
            .map(|pages| pages * 4)
            .unwrap_or(0)
    }
}

async fn serve(listener: tokio::net::TcpListener) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        tokio::spawn(async move {
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            loop {
                let mut content_length = 0usize;
                let mut line = String::new();
                let mut saw_request = false;
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) => return,
                        Ok(_) => {}
                        Err(_) => return,
                    }
                    saw_request = true;
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        break;
                    }
                    if let Some(v) = trimmed.to_ascii_lowercase().strip_prefix("content-length:") {
                        content_length = v.trim().parse().unwrap_or(0);
                    }
                }
                if !saw_request {
                    return;
                }
                let mut body = vec![0u8; content_length];
                if content_length > 0 && reader.read_exact(&mut body).await.is_err() {
                    return;
                }
                let req: serde_json::Value =
                    serde_json::from_slice(&body).unwrap_or_default();
                let result = match req["method"].as_str().unwrap_or_default() {
                    "initialize" => serde_json::json!({
                        "protocolVersion": "2024-11-05",
                        "capabilities": {},
                        "serverInfo": {"name": "mem", "version": "1"}
                    }),
                    "tools/list" => serde_json::json!({"tools": []}),
                    _ => {
                        let _ = writer
                            .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n")
                            .await;
                        continue;
                    }
                };
                let payload =
                    serde_json::json!({"jsonrpc":"2.0","id":req["id"],"result":result}).to_string();
                // Keep the connection alive so the measurement includes a live
                // pooled socket per server, the realistic idle state.
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{payload}",
                    payload.len()
                );
                if writer.write_all(response.as_bytes()).await.is_err() {
                    return;
                }
            }
        });
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "measures RSS; noisy and platform-specific"]
async fn idle_remote_connections_cost_little_memory() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    tokio::spawn(serve(listener));

    // Warm up one connection so allocator arenas and TLS setup are not counted
    // against the per-server cost.
    let warmup = McpClient::connect("warmup".to_string(), &make_config(&url))
        .await
        .expect("warmup connect");

    let before = rss_kb();

    let mut clients = Vec::with_capacity(SERVERS);
    for i in 0..SERVERS {
        clients.push(
            McpClient::connect(format!("mem-{i}"), &make_config(&url))
                .await
                .expect("connect"),
        );
    }

    // Let everything settle into the idle state being measured.
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let after = rss_kb();

    let delta = after.saturating_sub(before);
    let per_server = delta as f64 / SERVERS as f64;
    println!(
        "RSS before={before} KB, after={after} KB, delta={delta} KB for {SERVERS} idle remote servers => {per_server:.1} KB each"
    );

    assert_eq!(clients.len(), SERVERS, "all servers must stay connected");
    drop(warmup);

    // Measured at ~2 KB each once the HTTP client is shared. A per-server
    // `reqwest::Client` measured ~199 KB, so this ceiling is set well below
    // that to catch the regression coming back, while leaving RSS headroom.
    assert!(
        per_server < 25.0,
        "each idle remote MCP server should cost only a few KB, measured {per_server:.1} KB \
         (a per-server reqwest::Client regression costs ~199 KB)"
    );
}

fn make_config(url: &str) -> McpServerConfig {
    McpServerConfig {
        command: String::new(),
        args: Vec::new(),
        env: HashMap::new(),
        shared: false,
        transport: Some("http".to_string()),
        url: Some(url.to_string()),
        headers: HashMap::new(),
        oauth: None,
        enabled: None,
        disabled: None,
    }
}
