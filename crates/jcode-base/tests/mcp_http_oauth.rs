//! End-to-end test of the MCP OAuth flow against a real local server.
//!
//! The fake server implements the full chain jcode must traverse: an
//! unauthenticated `401` carrying a `WWW-Authenticate` challenge, the
//! protected-resource and authorization-server metadata documents, dynamic
//! client registration, the authorization endpoint (which redirects back to
//! jcode's loopback listener), and the token endpoint. A stand-in "browser"
//! fetches the authorization URL instead of a real one.
//!
//! This proves the flow works, that the resulting bearer token is actually
//! attached to MCP requests, and that credentials are persisted for reuse.

use jcode_base::mcp::{McpClient, McpServerConfig};
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
                    &serde_json::json!({"authorization_servers": [base]}).to_string(),
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
                    let parsed: serde_json::Value =
                        serde_json::from_str(&body).unwrap_or_default();
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
                    assert_eq!(
                        params.get("access_type").map(String::as_str),
                        Some("offline"),
                        "authorization must request offline access for refreshable credentials"
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
async fn unauthorized_server_triggers_oauth_and_then_connects() {
    // Redirect credential storage into a temp dir so the real ~/.jcode is
    // untouched.
    let home = tempfile::tempdir().expect("home tempdir");
    // SAFETY: single-threaded setup before any concurrent env access here.
    unsafe { std::env::set_var("JCODE_HOME", home.path()) };
    // Follow the authorization URL programmatically instead of opening a real
    // browser on the machine running the tests.
    unsafe { std::env::set_var("JCODE_MCP_AUTH_AUTOFOLLOW", "1") };

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base = format!("http://{addr}");
    let counters = Arc::new(Counters::default());
    tokio::spawn(serve(listener, base.clone(), Arc::clone(&counters)));

    let config = McpServerConfig {
        command: String::new(),
        args: Vec::new(),
        env: HashMap::new(),
        shared: false,
        transport: Some("http".to_string()),
        url: Some(format!("{base}/mcp")),
        headers: HashMap::new(),
        oauth: None,
        enabled: None,
        disabled: None,
    };

    let client = McpClient::connect("oauth-test".to_string(), &config)
        .await
        .expect("connect after completing the OAuth flow");

    // The server rejected us before we had a token, proving auth was required.
    assert!(
        counters.unauthorized.load(Ordering::SeqCst) >= 1,
        "the server must have challenged an unauthenticated request"
    );
    assert_eq!(
        counters.registrations.load(Ordering::SeqCst),
        1,
        "dynamic client registration must run exactly once"
    );

    assert_eq!(client.server_info().expect("server info").name, "oauth-server");
    assert_eq!(client.tools().len(), 1);
    assert_eq!(client.tools()[0].name, "secret");

    // Credentials persist for the next run, with the client id retained so a
    // second connect does not re-register.
    let stored = jcode_base::mcp::oauth::load_tokens("oauth-test").expect("tokens persisted");
    assert_eq!(stored.access_token, ACCESS_TOKEN);
    assert_eq!(stored.refresh_token.as_deref(), Some("test-refresh-token"));
    assert_eq!(stored.client_id.as_deref(), Some("test-client"));
    assert!(!stored.is_expired(), "a 3600s token must not read as expired");

    // A second connect with a still-valid token must not re-authorize at all:
    // no new 401 challenge beyond the first, and no new registration.
    let challenges_before = counters.unauthorized.load(Ordering::SeqCst);
    let _second = McpClient::connect("oauth-test".to_string(), &config)
        .await
        .expect("reconnect with stored credentials");
    assert_eq!(
        counters.unauthorized.load(Ordering::SeqCst),
        challenges_before,
        "a valid stored token must be sent on the first request, avoiding a 401"
    );

    // Now force a genuine re-authorization: keep the registered client_id and
    // redirect URI but make the access token invalid and remove any refresh
    // token, so the only way back in is a fresh authorization. Both values must
    // be reused rather than registering a second client or changing the URI.
    jcode_base::mcp::oauth::save_tokens(
        "oauth-test",
        &jcode_base::mcp::oauth::McpOAuthTokens {
            access_token: "stale-token".to_string(),
            refresh_token: None,
            expires_at: 0,
            client_id: stored.client_id.clone(),
            token_endpoint: stored.token_endpoint.clone(),
            redirect_uri: stored.redirect_uri.clone(),
        },
    )
    .expect("seed stale credentials");

    let _third = McpClient::connect("oauth-test".to_string(), &config)
        .await
        .expect("reconnect by re-authorizing with the stored client_id");
    assert!(
        counters.unauthorized.load(Ordering::SeqCst) > challenges_before,
        "the stale token must have been rejected, forcing re-authorization"
    );
    assert_eq!(
        counters.registrations.load(Ordering::SeqCst),
        1,
        "re-authorization must reuse the stored client_id instead of registering again"
    );

    unsafe { std::env::remove_var("JCODE_HOME") };
}
