//! Live OAuth check against a real remote MCP server.
//!
//! Ignored by default because it opens a browser and needs a human to sign in.
//! Run it deliberately:
//!
//! ```sh
//! JCODE_LIVE_MCP_URL=https://mcp.granola.ai/mcp \
//!   cargo test -p jcode-base --profile selfdev --test mcp_live_oauth -- --ignored --nocapture
//! ```

use jcode_base::mcp::{McpClient, McpServerConfig};
use std::collections::HashMap;

#[tokio::test]
#[ignore = "opens a browser and requires interactive sign-in"]
async fn live_remote_server_completes_oauth_and_lists_tools() {
    let url = std::env::var("JCODE_LIVE_MCP_URL")
        .expect("set JCODE_LIVE_MCP_URL to the remote MCP endpoint");
    let name = std::env::var("JCODE_LIVE_MCP_NAME").unwrap_or_else(|_| "live-test".to_string());

    let config = McpServerConfig {
        command: String::new(),
        args: Vec::new(),
        env: HashMap::new(),
        shared: false,
        transport: Some("http".to_string()),
        url: Some(url.clone()),
        headers: HashMap::new(),
        oauth: None,
        enabled: None,
        disabled: None,
    };

    println!("Connecting to {url} ...");
    let client = McpClient::connect(name.clone(), &config)
        .await
        .expect("connect to the live server");

    let info = client.server_info().expect("server info");
    println!("Connected to {} v{:?}", info.name, info.version);

    let tools = client.tools();
    println!("Discovered {} tools:", tools.len());
    for tool in &tools {
        println!(
            "  - {}: {}",
            tool.name,
            tool.description.as_deref().unwrap_or("")
        );
    }
    assert!(!tools.is_empty(), "a live server should advertise tools");

    // Reconnecting must reuse the stored token with no second sign-in.
    let again = McpClient::connect(name, &config)
        .await
        .expect("reconnect using stored credentials");
    assert_eq!(
        again.tools().len(),
        tools.len(),
        "the cached token should yield the same tool list without re-authorizing"
    );
    println!("Reconnected using the cached token, no second sign-in.");
}
