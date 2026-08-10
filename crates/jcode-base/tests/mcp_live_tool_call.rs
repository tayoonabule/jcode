//! Calls a real tool on a live remote MCP server using cached credentials.
//!
//! Ignored by default. Requires a prior sign-in (see `mcp_live_oauth`), then:
//!
//! ```sh
//! JCODE_LIVE_MCP_URL=https://mcp.granola.ai/mcp JCODE_LIVE_MCP_NAME=granola \
//! JCODE_LIVE_MCP_TOOL=get_account_info \
//!   cargo test -p jcode-base --profile selfdev --test mcp_live_tool_call -- --ignored --nocapture
//! ```

use jcode_base::mcp::{ContentBlock, McpClient, McpServerConfig};
use std::collections::HashMap;

#[tokio::test]
#[ignore = "requires cached credentials for a live remote MCP server"]
async fn live_remote_tool_call_returns_content() {
    let url = std::env::var("JCODE_LIVE_MCP_URL").expect("set JCODE_LIVE_MCP_URL");
    let name = std::env::var("JCODE_LIVE_MCP_NAME").unwrap_or_else(|_| "live-test".to_string());
    let tool = std::env::var("JCODE_LIVE_MCP_TOOL").expect("set JCODE_LIVE_MCP_TOOL");

    let config = McpServerConfig {
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
    };

    let client = McpClient::connect(name, &config)
        .await
        .expect("connect with cached credentials");

    let result = client
        .call_tool(&tool, serde_json::json!({}))
        .await
        .unwrap_or_else(|e| panic!("calling '{tool}' failed: {e:#}"));

    assert!(
        !result.content.is_empty(),
        "'{tool}' returned no content at all"
    );

    let mut total = 0usize;
    for block in &result.content {
        if let ContentBlock::Text { text } = block {
            total += text.len();
            let preview: String = text.chars().take(600).collect();
            println!("--- {tool} returned {} chars ---\n{preview}", text.len());
        }
    }
    assert!(
        total > 0,
        "'{tool}' returned content blocks but no text payload"
    );
    assert!(
        !result.is_error,
        "'{tool}' reported an error result"
    );
}
