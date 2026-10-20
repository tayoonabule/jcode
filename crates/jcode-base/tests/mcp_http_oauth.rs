//! End-to-end test of the explicit MCP OAuth login flow against local fixtures.
//!
//! The fake authorization server implements protected-resource metadata,
//! authorization-server metadata, dynamic client registration, the PKCE
//! authorization redirect, and token exchange. The test follows the printed
//! authorization URL programmatically rather than opening a real browser.
//!
//! This exercises the current public login and connection APIs, verifies the
//! bearer token is sent to MCP, and checks credentials are persisted/reused.

use jcode_base::mcp::{McpClient, McpServerConfig, oauth::BrowserMode};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

const ACCESS_TOKEN: &str = "test-access-token";

#[derive(Default)]
struct Counters {
    registrations: AtomicUsize,
    unauthorized: AtomicUsize,
}

fn write_response(body: &str, content_type: &str) -> String {
    format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}

/// A fake MCP server plus its co-located authorization server.
async fn serve(listener: tokio::net::TcpListener, base: String, counters: Arc<Counters>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        let base = base.clone();
        let counters = Arc::clone(&counters);
        tokio::spawn(async move {
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);

            let mut request_line = String::new();
            if reader.read_line(&mut request_line).await.unwrap_or(0) == 0 {
                return;
            }
            let mut parts = request_line.split_whitespace();
            let _method = parts.next().unwrap_or_default().to_string();
            let target = parts.next().unwrap_or_default().to_string();

            let mut content_length = 0usize;
            let mut authorization = String::new();
            let mut line = String::new();
            loop {
                line.clear();
                if reader.read_line(&mut line).await.unwrap_or(0) == 0 {
                    return;
                }
                let trimmed = line.trim().to_string();
                if trimmed.is_empty() {
                    break;
                }
                let lower = trimmed.to_ascii_lowercase();
                if let Some(value) = lower.strip_prefix("content-length:") {
                    content_length = value.trim().parse().unwrap_or(0);
                } else if let Some(value) = lower.strip_prefix("authorization:") {
                    authorization = value.trim().to_string();
                }
            }

            let mut body = vec![0u8; content_length];
            if content_length > 0 && reader.read_exact(&mut body).await.is_err() {
                return;
            }
            let body = String::from_utf8_lossy(&body).to_string();

            let path = target.split('?').next().unwrap_or_default();
            let query = target.split_once('?').map(|(_, q)| q).unwrap_or_default();

            let response = match path {
                "/.well-known/oauth-protected-resource" => write_response(
                    &serde_json::json!({
                        "resource": format!("{base}/mcp"),
                        "authorization_servers": [base]
                    })
                    .to_string(),
                    "application/json",
                ),
                "/.well-known/oauth-authorization-server" => write_response(
                    &serde_json::json!({
                        "issuer": base,
                        "authorization_endpoint": format!("{base}/authorize"),
                        "token_endpoint": format!("{base}/token"),
                        "registration_endpoint": format!("{base}/register"),
                    })
                    .to_string(),
                    "application/json",
                ),
                "/register" => {
                    counters.registrations.fetch_add(1, Ordering::SeqCst);
                    // The client must offer a loopback redirect URI.
                    let parsed: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
                    assert!(
                        parsed["redirect_uris"][0]
                            .as_str()
                            .unwrap_or_default()
                            .starts_with("http://127.0.0.1:"),
                        "registration must use a loopback redirect: {body}"
                    );
                    write_response(
                        &serde_json::json!({"client_id": "test-client"}).to_string(),
                        "application/json",
                    )
                }
                "/authorize" => {
                    // Echo state back to jcode's loopback listener, as a real
                    // authorization server would after the user signs in.
                    let params: HashMap<_, _> = url::form_urlencoded::parse(query.as_bytes())
                        .into_owned()
                        .collect();
                    assert_eq!(
                        params.get("code_challenge_method").map(String::as_str),
                        Some("S256"),
                        "PKCE must be S256"
                    );
                    assert!(
                        params.contains_key("code_challenge"),
                        "PKCE challenge must be present"
                    );
                    let redirect = params.get("redirect_uri").cloned().unwrap_or_default();
                    let state = params.get("state").cloned().unwrap_or_default();
                    let callback = format!("{redirect}?code=test-code&state={state}");
                    // The "browser" follows the redirect itself.
                    let _ = reqwest::get(&callback).await;
                    write_response("ok", "text/plain")
                }
                "/token" => {
                    let params: HashMap<_, _> = url::form_urlencoded::parse(body.as_bytes())
                        .into_owned()
                        .collect();
                    assert!(
                        params.contains_key("code_verifier"),
                        "token exchange must send the PKCE verifier"
                    );
                    write_response(
                        &serde_json::json!({
                            "access_token": ACCESS_TOKEN,
                            "refresh_token": "test-refresh-token",
                            "token_type": "Bearer",
                            "expires_in": 3600,
                        })
                        .to_string(),
                        "application/json",
                    )
                }
                "/mcp" => {
                    if authorization != format!("bearer {ACCESS_TOKEN}").to_ascii_lowercase()
                        && authorization != format!("Bearer {ACCESS_TOKEN}")
                    {
                        counters.unauthorized.fetch_add(1, Ordering::SeqCst);
                        let challenge = format!(
                            "Bearer error=\"invalid_token\", resource_metadata=\"{base}/.well-known/oauth-protected-resource\""
                        );
                        let _ = writer
                            .write_all(
                                format!(
                                    "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: {challenge}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                                )
                                .as_bytes(),
                            )
                            .await;
                        return;
                    }

                    let request: serde_json::Value =
                        serde_json::from_str(&body).unwrap_or_default();
                    let method = request["method"].as_str().unwrap_or_default();
                    let result = match method {
                        "initialize" => serde_json::json!({
                            "protocolVersion": "2024-11-05",
                            "capabilities": {},
                            "serverInfo": {"name": "oauth-server", "version": "1"}
                        }),
                        "tools/list" => serde_json::json!({
                            "tools": [{
                                "name": "secret",
                                "description": "Needs auth",
                                "inputSchema": {"type": "object"}
                            }]
                        }),
                        _ => {
                            let _ = writer
                                .write_all(b"HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n")
                                .await;
                            return;
                        }
                    };
                    write_response(
                        &serde_json::json!({
                            "jsonrpc": "2.0", "id": request["id"], "result": result
                        })
                        .to_string(),
                        "application/json",
                    )
                }
                _ => "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_string(),
            };

            let _ = writer.write_all(response.as_bytes()).await;
        });
    }
}

#[tokio::test]
async fn explicit_oauth_login_persists_credentials_and_connects() {
    // Keep test credentials isolated from the real user store.
    let home = tempfile::tempdir().expect("home tempdir");
    let previous_home = std::env::var_os("JCODE_HOME");
    // SAFETY: this is the only environment mutation in this test binary.
    unsafe { std::env::set_var("JCODE_HOME", home.path()) };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let counters = Arc::new(Counters::default());
    tokio::spawn(serve(listener, base.clone(), Arc::clone(&counters)));

    let mcp_url = format!("{base}/mcp");
    let config = McpServerConfig {
        command: String::new(),
        args: Vec::new(),
        env: HashMap::new(),
        shared: false,
        transport: Some("http".to_string()),
        url: Some(mcp_url.clone()),
        headers: HashMap::new(),
        timeout_secs: None,
        enabled: None,
        disabled: None,
    };

    // Explicit login receives the authorization URL; simulate a user following
    // it in the local fixture rather than opening a real browser.
    tokio::time::timeout(
        std::time::Duration::from_secs(10),
        jcode_base::mcp::oauth::login(&mcp_url, BrowserMode::Print, |auth_url| {
            let auth_url = auth_url.to_string();
            tokio::spawn(async move {
                let _ = reqwest::get(auth_url).await;
            });
        }),
    )
    .await
    .expect("OAuth fixture callback should complete promptly")
    .expect("complete explicit OAuth flow");

    let client = McpClient::connect("oauth-test".to_string(), &config)
        .await
        .expect("connect with saved OAuth credentials");
    assert_eq!(counters.registrations.load(Ordering::SeqCst), 1);
    assert_eq!(
        client.server_info().expect("server info").name,
        "oauth-server"
    );
    assert_eq!(client.tools().len(), 1);
    assert_eq!(client.tools()[0].name, "secret");

    let stored = jcode_base::mcp::oauth::stored(&mcp_url).expect("credentials persisted");
    assert_eq!(stored.access_token.as_deref(), Some(ACCESS_TOKEN));
    assert_eq!(stored.refresh_token.as_deref(), Some("test-refresh-token"));
    assert_eq!(stored.client_id, "test-client");
    assert!(stored.expires_at.is_some());

    let _second = McpClient::connect("oauth-test".to_string(), &config)
        .await
        .expect("reconnect with stored credentials");
    assert_eq!(counters.registrations.load(Ordering::SeqCst), 1);
    // The explicit OAuth discovery probe is intentionally unauthenticated; the
    // actual MCP connection and subsequent reconnect use the saved token.
    assert_eq!(counters.unauthorized.load(Ordering::SeqCst), 1);

    if let Some(previous_home) = previous_home {
        unsafe { std::env::set_var("JCODE_HOME", previous_home) };
    } else {
        unsafe { std::env::remove_var("JCODE_HOME") };
    }
}
