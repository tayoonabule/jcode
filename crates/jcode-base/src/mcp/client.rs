//! MCP Client - handles communication with a single MCP server

use super::pending::{self, PendingMap};
use super::protocol::*;
use anyhow::{Context, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::future::Future;
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::mpsc;

/// How a handle talks to its server.
///
/// Kept as an enum rather than a boxed trait object so the stdio path stays
/// allocation-free per message and an HTTP server costs one `Arc`.
#[derive(Clone)]
enum Transport {
    Stdio {
        pending: PendingMap,
        writer_tx: mpsc::Sender<String>,
        /// Set by the reader on stdout EOF: no reply can arrive, so requests fail fast.
        closed: Arc<AtomicBool>,
    },
    Http(Arc<super::http::HttpTransport>),
    Sse(Arc<super::sse::SseTransport>),
    /// A shared stdio server owned by the machine-wide broker process.
    #[cfg(unix)]
    Broker(Arc<super::broker::BrokerTransport>),
}

/// Shared communication handle for an MCP server.
/// Multiple sessions can hold clones of this and send concurrent requests.
/// Request/response correlation by ID ensures no interference.
#[derive(Clone)]
pub struct McpHandle {
    pub(crate) name: String,
    request_id: Arc<AtomicU64>,
    transport: Transport,
    server_info: Arc<std::sync::RwLock<Option<ServerInfo>>>,
    capabilities: Arc<std::sync::RwLock<ServerCapabilities>>,
    tools: Arc<std::sync::RwLock<Vec<McpToolDef>>>,
    /// Reply timeout applied to every request on this server.
    request_timeout: std::time::Duration,
}

/// Default reply timeout when a server config does not set `timeout_secs`.
pub const DEFAULT_MCP_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// Resolve the per-request reply timeout for a server config.
pub fn request_timeout_for(config: &McpServerConfig) -> std::time::Duration {
    config
        .timeout_secs
        .filter(|secs| *secs > 0)
        .map(std::time::Duration::from_secs)
        .unwrap_or(DEFAULT_MCP_REQUEST_TIMEOUT)
}

/// Whether a successful tool result is really an authentication failure.
///
/// Some first-party gateways (Google's, notably) answer HTTP 200 with an
/// `isError` result whose text says the credential is missing or expired,
/// instead of returning a 401 the transport could act on.
fn is_auth_error_result(result: &ToolCallResult) -> bool {
    result.is_error
        && result.content.iter().any(|content| {
            matches!(content, ContentBlock::Text { text } if super::http::is_auth_error_text(text))
        })
}

impl McpHandle {
    /// Send a request and wait for response
    pub async fn request(&self, method: &str, params: Option<Value>) -> Result<JsonRpcResponse> {
        let id = self.request_id.fetch_add(1, Ordering::SeqCst);
        let request = JsonRpcRequest::new(id, method, params);
        let body = serde_json::to_string(&request)?;

        // Every transport gets the same reply deadline. Remote transports used
        // to rely on their own hardcoded constant (SSE) or none at all (HTTP),
        // which meant a hung remote server could block the calling tool forever
        // and silently ignored the server's configured `timeout_secs`.
        let exchange = async {
            match &self.transport {
                Transport::Http(http) => http
                    .send(&body, true)
                    .await?
                    .context("MCP server returned no JSON-RPC response"),
                Transport::Sse(sse) => sse
                    .send(&body, id, true)
                    .await?
                    .context("MCP server returned no JSON-RPC response"),
                #[cfg(unix)]
                Transport::Broker(broker) => broker
                    .send(&body, id, true)
                    .await?
                    .context("MCP server returned no JSON-RPC response"),
                Transport::Stdio {
                    pending,
                    writer_tx,
                    closed,
                } => {
                    // Register before sending so a reply that arrives while we
                    // are still awaiting the writer still finds its waiter, and
                    // so the deadline below cannot leave the slot behind: the
                    // registration is released when this future is dropped.
                    let waiter = pending::PendingRequest::register(pending, id).await;
                    // Checked after registering: the reader sets `closed` before
                    // failing every waiter, so a request registered after that
                    // sweep still sees the flag instead of waiting out its deadline.
                    if closed.load(Ordering::SeqCst) {
                        waiter.cancel().await;
                        anyhow::bail!("MCP server '{}' exited (stdout closed)", self.name);
                    }
                    if let Err(error) = writer_tx.send(body + "\n").await {
                        waiter.cancel().await;
                        return Err(error).context("Failed to send request");
                    }
                    waiter.recv().await.with_context(|| {
                        format!("MCP server '{}' exited (stdout closed)", self.name)
                    })
                }
            }
        };

        let response = self.await_reply(exchange).await?;

        if let Some(err) = &response.error {
            anyhow::bail!("MCP error {}: {}", err.code, err.message);
        }

        Ok(response)
    }

    /// Await one exchange under this server's reply deadline.
    ///
    /// The deadline is suspended while a remote transport is waiting on the
    /// user to finish a browser sign-in. That wait is triggered from inside a
    /// request but is bounded by a person, not the server, so counting it would
    /// make the first call to an unauthorized remote server fail after 30s no
    /// matter how promptly the server itself responds.
    async fn await_reply(
        &self,
        exchange: impl Future<Output = Result<JsonRpcResponse>>,
    ) -> Result<JsonRpcResponse> {
        let mut exchange = std::pin::pin!(exchange);
        loop {
            match tokio::time::timeout(self.request_timeout, &mut exchange).await {
                Ok(response) => return response,
                Err(_) if super::http::interactive_auth_in_progress(&self.name) => continue,
                Err(elapsed) => {
                    return Err(elapsed).with_context(|| {
                        format!(
                            "Request timeout after {}s (raise `timeout_secs` for MCP server '{}' if its tools legitimately run longer)",
                            self.request_timeout.as_secs(),
                            self.name
                        )
                    });
                }
            }
        }
    }

    /// Send a notification (no response expected).
    async fn notify(&self, method: &str, params: Option<Value>) -> Result<()> {
        let body = serde_json::to_string(&JsonRpcNotification::new(method, params))?;
        match &self.transport {
            Transport::Http(http) => {
                http.send(&body, false).await?;
            }
            Transport::Sse(sse) => {
                sse.notify(&body).await?;
            }
            #[cfg(unix)]
            Transport::Broker(broker) => {
                broker.notify(&body).await?;
            }
            Transport::Stdio { writer_tx, .. } => {
                writer_tx.send(body + "\n").await?;
            }
        }
        Ok(())
    }

    /// Call a tool
    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolCallResult> {
        let arguments = if arguments.is_null() {
            Value::Object(serde_json::Map::new())
        } else {
            arguments
        };
        let params = ToolCallParams {
            name: name.to_string(),
            arguments,
        };

        // Some first-party gateways report an expired credential as a normal
        // `isError` tool result rather than a 401, so one retry is allowed
        // after refreshing the remote transport's credentials.
        let mut may_reauthenticate = true;
        loop {
            let response = self
                .request("tools/call", Some(serde_json::to_value(&params)?))
                .await?;

            let result = response.result.context("No result from tool call")?;
            let tool_result: ToolCallResult = serde_json::from_value(result)?;
            if may_reauthenticate && is_auth_error_result(&tool_result) {
                may_reauthenticate = false;
                match &self.transport {
                    Transport::Http(http) => {
                        http.reauthenticate().await?;
                        continue;
                    }
                    Transport::Sse(sse) => {
                        sse.reauthenticate().await?;
                        continue;
                    }
                    Transport::Stdio { .. } => {}
                    #[cfg(unix)]
                    Transport::Broker(_) => {}
                }
            }
            return Ok(tool_result);
        }
    }

    /// Get the server name
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get server info
    pub fn server_info(&self) -> Option<ServerInfo> {
        self.server_info
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Get available tools
    pub fn tools(&self) -> Vec<McpToolDef> {
        self.tools
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// Refresh the list of available tools
    pub async fn refresh_tools(&self) -> Result<()> {
        let response = self.request("tools/list", None).await?;

        if let Some(result) = response.result {
            let tools_result: ToolsListResult = serde_json::from_value(result)?;
            *self
                .tools
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = tools_result.tools;
        }

        Ok(())
    }
}

/// MCP Client - owns the child process and provides shared handles.
/// Only one McpClient exists per MCP server process, but many McpHandle
/// clones can be distributed to different sessions.
pub struct McpClient {
    handle: McpHandle,
    /// `None` for remote (HTTP) servers, which have no child process.
    child: Option<Child>,
}

impl McpClient {
    /// Connect to an MCP server, inheriting the current process working directory
    pub async fn connect(name: String, config: &McpServerConfig) -> Result<Self> {
        Self::connect_in_dir(name, config, None).await
    }

    /// Connect a server that every jcode daemon on this machine may share.
    ///
    /// Stdio servers are routed through the machine-wide broker so all
    /// daemons reuse one child process. If the broker cannot be reached or
    /// started, the server is spawned directly as before, so a broker problem
    /// can cost memory but never MCP availability.
    pub async fn connect_shared(name: String, config: &McpServerConfig) -> Result<Self> {
        #[cfg(unix)]
        if config.is_stdio() && super::broker::broker_available() {
            match super::broker::socket_path() {
                Ok(socket) => {
                    match Self::connect_via_broker(name.clone(), config, socket, true).await {
                        Ok(client) => return Ok(client),
                        Err(super::broker::BrokerConnectError::Upstream(error)) => {
                            return Err(error)
                                .with_context(|| format!("MCP server '{name}' failed to start"));
                        }
                        Err(error) => crate::logging::warn(&format!(
                            "MCP: {error}; spawning '{name}' directly"
                        )),
                    }
                }
                Err(error) => crate::logging::warn(&format!(
                    "MCP: no broker socket path ({error:#}); spawning '{name}' directly"
                )),
            }
        }
        Self::connect_in_dir(name, config, None).await
    }

    /// Attach to `name` through the broker listening at `socket`.
    #[cfg(unix)]
    pub async fn connect_via_broker(
        name: String,
        config: &McpServerConfig,
        socket: std::path::PathBuf,
        autostart: bool,
    ) -> std::result::Result<Self, super::broker::BrokerConnectError> {
        use super::broker::{BrokerConnectError, BrokerTransport};
        let transport = BrokerTransport::connect(name.clone(), config, socket, autostart).await?;
        let handle = McpHandle {
            name: name.clone(),
            request_id: Arc::new(AtomicU64::new(1)),
            transport: Transport::Broker(Arc::new(transport)),
            server_info: Arc::new(std::sync::RwLock::new(None)),
            capabilities: Arc::new(std::sync::RwLock::new(ServerCapabilities::default())),
            tools: Arc::new(std::sync::RwLock::new(Vec::new())),
            request_timeout: request_timeout_for(config),
        };
        let mut client = Self {
            handle,
            child: None,
        };
        // The broker is up and the upstream answered its own handshake, so a
        // failure from here on is the server's, not the broker's.
        client
            .initialize()
            .await
            .with_context(|| format!("MCP server '{name}' failed to initialize"))
            .map_err(BrokerConnectError::Upstream)?;
        client
            .handle
            .refresh_tools()
            .await
            .with_context(|| format!("MCP server '{name}' failed to list tools"))
            .map_err(BrokerConnectError::Upstream)?;
        crate::logging::info(&format!(
            "MCP: Connected to '{}' via broker with {} tools",
            name,
            client.handle.tools().len()
        ));
        Ok(client)
    }

    /// Connect to an MCP server, optionally running it in `working_dir`.
    ///
    /// The working directory is only applied when it exists; otherwise the
    /// subprocess falls back to inheriting the current process cwd (issue #557).
    pub async fn connect_in_dir(
        name: String,
        config: &McpServerConfig,
        working_dir: Option<&std::path::Path>,
    ) -> Result<Self> {
        if !config.is_stdio() {
            if config
                .transport
                .as_deref()
                .is_some_and(|transport| transport.eq_ignore_ascii_case("sse"))
            {
                return Self::connect_sse(name, config).await;
            }
            return Self::connect_http(name, config).await;
        }
        let working_dir = working_dir.filter(|dir| dir.is_dir());
        crate::logging::info(&format!(
            "MCP: Connecting to '{}' ({} {:?}) cwd={:?}",
            name, config.command, config.args, working_dir
        ));

        // Credentials must be opted into an MCP server explicitly through its
        // config. The long-lived jcode daemon contains provider credentials in
        // its process environment, and blindly inheriting them exposes those
        // credentials to every configured MCP executable (issue #771).
        let inherited: HashMap<String, String> = std::env::vars().collect();
        let env = mcp_child_env(inherited, &config.env);

        let mut command = Command::new(&config.command);
        command
            .args(&config.args)
            .envs(&env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(dir) = working_dir {
            command.current_dir(dir);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("Failed to spawn MCP server: {}", config.command))?;

        let stdin = child.stdin.take().context("No stdin")?;
        let stdout = child.stdout.take().context("No stdout")?;
        let stderr = child.stderr.take().context("No stderr")?;

        // Spawn stderr reader
        let server_name = name.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => break,
                    Ok(_) => {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            crate::logging::warn(&format!(
                                "MCP [{}] stderr: {}",
                                server_name, trimmed
                            ));
                        }
                    }
                    Err(_) => break,
                }
            }
        });

        // Setup channels
        let pending = pending::new_pending();
        let (writer_tx, mut writer_rx) = mpsc::channel::<String>(32);

        // Spawn writer task
        let mut stdin = stdin;
        tokio::spawn(async move {
            while let Some(msg) = writer_rx.recv().await {
                if stdin.write_all(msg.as_bytes()).await.is_err() {
                    break;
                }
                if stdin.flush().await.is_err() {
                    break;
                }
            }
        });

        // Spawn reader task
        let pending_clone = Arc::clone(&pending);
        let closed = Arc::new(AtomicBool::new(false));
        let closed_clone = Arc::clone(&closed);
        let reader_name = name.clone();
        let mut reader = BufReader::new(stdout);
        tokio::spawn(async move {
            let mut line = String::new();
            loop {
                line.clear();
                match reader.read_line(&mut line).await {
                    Ok(0) => {
                        crate::logging::debug(&format!("MCP [{}]: stdout EOF", reader_name));
                        break;
                    }
                    Ok(_) => {
                        if let Ok(response) = serde_json::from_str::<JsonRpcResponse>(&line) {
                            pending::resolve(&pending_clone, response).await;
                        } else {
                            let trimmed = line.trim();
                            if !trimmed.is_empty() {
                                crate::logging::debug(&format!(
                                    "MCP [{}] non-JSON output: {}",
                                    reader_name, trimmed
                                ));
                            }
                        }
                    }
                    Err(e) => {
                        crate::logging::warn(&format!("MCP [{}] read error: {}", reader_name, e));
                        break;
                    }
                }
            }
            // Nothing will answer the outstanding requests now that the child's
            // stdout is gone, so wake their callers instead of making each one
            // sit out its full reply deadline.
            closed_clone.store(true, Ordering::SeqCst);
            pending::fail_all(&pending_clone).await;
        });

        let handle = McpHandle {
            name: name.clone(),
            request_id: Arc::new(AtomicU64::new(1)),
            transport: Transport::Stdio {
                pending,
                writer_tx,
                closed,
            },
            server_info: Arc::new(std::sync::RwLock::new(None)),
            capabilities: Arc::new(std::sync::RwLock::new(ServerCapabilities::default())),
            tools: Arc::new(std::sync::RwLock::new(Vec::new())),
            request_timeout: request_timeout_for(config),
        };

        let mut client = Self {
            handle,
            child: Some(child),
        };

        client
            .initialize()
            .await
            .with_context(|| format!("MCP server '{}' failed to initialize", name))?;

        client
            .handle
            .refresh_tools()
            .await
            .with_context(|| format!("MCP server '{}' failed to list tools", name))?;

        crate::logging::info(&format!(
            "MCP: Connected to '{}' with {} tools",
            name,
            client.handle.tools().len()
        ));

        Ok(client)
    }

    /// Connect to a remote MCP server over Streamable HTTP.
    async fn connect_http(name: String, config: &McpServerConfig) -> Result<Self> {
        let url = config.url.as_deref().unwrap_or_default();
        crate::logging::info(&format!("MCP: Connecting to '{name}' over HTTP ({url})"));

        let transport = super::http::HttpTransport::new(name.clone(), config)?;
        Self::start_remote(name, config, "HTTP", Transport::Http(Arc::new(transport))).await
    }

    /// Connect to a legacy MCP server using a long-lived SSE GET stream and
    /// the POST endpoint announced by its initial `endpoint` event.
    async fn connect_sse(name: String, config: &McpServerConfig) -> Result<Self> {
        let url = config.url.as_deref().unwrap_or_default();
        crate::logging::info(&format!("MCP: Connecting to '{name}' over SSE ({url})"));

        let transport = super::sse::SseTransport::new(name.clone(), config)?;
        transport.connect().await?;
        Self::start_remote(name, config, "SSE", Transport::Sse(Arc::new(transport))).await
    }

    /// Bring up a remote server: handshake, then load its tools.
    ///
    /// Remote transports differ only in how they carry bytes, so both share
    /// this to avoid the two paths drifting apart in what they initialize.
    async fn start_remote(
        name: String,
        config: &McpServerConfig,
        label: &str,
        transport: Transport,
    ) -> Result<Self> {
        let handle = McpHandle {
            name: name.clone(),
            request_id: Arc::new(AtomicU64::new(1)),
            transport,
            server_info: Arc::new(std::sync::RwLock::new(None)),
            capabilities: Arc::new(std::sync::RwLock::new(ServerCapabilities::default())),
            tools: Arc::new(std::sync::RwLock::new(Vec::new())),
            request_timeout: request_timeout_for(config),
        };

        let mut client = Self {
            handle,
            child: None,
        };
        client
            .initialize()
            .await
            .with_context(|| format!("MCP server '{name}' failed to initialize"))?;
        client
            .handle
            .refresh_tools()
            .await
            .with_context(|| format!("MCP server '{name}' failed to list tools"))?;

        crate::logging::info(&format!(
            "MCP: Connected to '{}' over {} with {} tools",
            name,
            label,
            client.handle.tools().len()
        ));
        Ok(client)
    }

    /// Get a shareable handle to this client
    pub fn handle(&self) -> McpHandle {
        self.handle.clone()
    }

    /// Initialize the MCP connection
    async fn initialize(&mut self) -> Result<()> {
        let params = InitializeParams {
            protocol_version: "2024-11-05".to_string(),
            capabilities: ClientCapabilities::default(),
            client_info: ClientInfo {
                name: "jcode".to_string(),
                version: jcode_build_meta::pkg_version().to_string(),
            },
        };

        let response = self
            .handle
            .request("initialize", Some(serde_json::to_value(params)?))
            .await?;

        if let Some(result) = response.result {
            let init_result: InitializeResult = serde_json::from_value(result)?;
            *self
                .handle
                .server_info
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = init_result.server_info;
            *self
                .handle
                .capabilities
                .write()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = init_result.capabilities;
        }

        // Send initialized notification
        self.handle
            .notify("notifications/initialized", None)
            .await?;

        Ok(())
    }

    /// Check if server is still running
    pub fn is_running(&mut self) -> bool {
        let Some(child) = self.child.as_mut() else {
            // Remote servers are stateless from our side; liveness is checked
            // per request instead of by process status.
            return true;
        };
        match child.try_wait() {
            Ok(None) => true,
            Ok(Some(_)) => false,
            Err(_) => false,
        }
    }

    /// Shutdown the server
    pub async fn shutdown(&mut self) {
        let _ = self.handle.notify("shutdown", None).await;
        if let Some(child) = self.child.as_mut() {
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            let _ = child.kill().await;
        }
    }

    // === Legacy compatibility methods that delegate to handle ===

    pub fn name(&self) -> &str {
        &self.handle.name
    }

    pub fn server_info(&self) -> Option<ServerInfo> {
        self.handle.server_info()
    }

    pub fn tools(&self) -> Vec<McpToolDef> {
        self.handle.tools()
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> Result<ToolCallResult> {
        self.handle.call_tool(name, arguments).await
    }

    pub async fn refresh_tools(&self) -> Result<()> {
        self.handle.refresh_tools().await
    }
}

/// Secrets that an MCP child must not receive merely because jcode has them.
///
/// This intentionally applies only to inherited values. A server can still be
/// given any of these names through `McpServerConfig::env`.
fn is_sensitive_inherited_env_key(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    key.ends_with("_API_KEY")
        || key.ends_with("_ACCESS_TOKEN")
        || key.ends_with("_AUTH_TOKEN")
        || matches!(
            key.as_str(),
            "AWS_ACCESS_KEY_ID"
                | "AWS_SECRET_ACCESS_KEY"
                | "AWS_SESSION_TOKEN"
                | "AZURE_CLIENT_SECRET"
                | "GOOGLE_APPLICATION_CREDENTIALS"
        )
}

pub(super) fn mcp_child_env(
    mut inherited: HashMap<String, String>,
    explicit: &HashMap<String, String>,
) -> HashMap<String, String> {
    inherited.retain(|key, _| !is_sensitive_inherited_env_key(key));
    inherited.extend(explicit.clone());
    inherited
}

impl Drop for McpClient {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.start_kill();
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::http::is_auth_error_text;
    use super::{McpClient, is_sensitive_inherited_env_key, mcp_child_env};
    use crate::mcp::protocol::McpServerConfig;
    use std::collections::HashMap;

    #[test]
    fn inherited_mcp_env_scrubs_provider_credentials() {
        for key in [
            "ANTHROPIC_API_KEY",
            "openai_api_key",
            "CURSOR_ACCESS_TOKEN",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "GOOGLE_APPLICATION_CREDENTIALS",
        ] {
            assert!(is_sensitive_inherited_env_key(key), "must scrub {key}");
        }
        for key in ["PATH", "HOME", "RUST_LOG", "JCODE_OPENROUTER_API_KEY_NAME"] {
            assert!(!is_sensitive_inherited_env_key(key), "must preserve {key}");
        }
    }

    #[test]
    fn explicit_mcp_env_can_opt_a_credential_back_in() {
        let inherited = HashMap::from([
            ("PATH".to_string(), "/bin".to_string()),
            ("ANTHROPIC_API_KEY".to_string(), "daemon-secret".to_string()),
        ]);
        let explicit = HashMap::from([(
            "ANTHROPIC_API_KEY".to_string(),
            "server-specific-secret".to_string(),
        )]);

        let env = mcp_child_env(inherited, &explicit);
        assert_eq!(env.get("PATH").map(String::as_str), Some("/bin"));
        assert_eq!(
            env.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("server-specific-secret")
        );
    }

    #[test]
    fn recognizes_google_style_application_auth_errors_only() {
        assert!(is_auth_error_text(
            "Request is missing required authentication credential. Expected OAuth 2 access token"
        ));
        assert!(is_auth_error_text(
            "Expected OAuth 2 access token or login cookie"
        ));
        assert!(is_auth_error_text(
            "Method doesn't allow unregistered callers without established identity"
        ));
        assert!(!is_auth_error_text("The requested message was not found"));
    }

    /// A minimal fake stdio MCP server (shell script) that reports its own
    /// process cwd as the serverInfo name.
    fn fake_server_config() -> McpServerConfig {
        let script = r#"
while IFS= read -r line; do
  case "$line" in
    *'"initialize"'*)
      printf '{"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2024-11-05","capabilities":{},"serverInfo":{"name":"%s","version":"0"}}}\n' "$PWD"
      ;;
    *'"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":2,"result":{"tools":[]}}\n'
      ;;
  esac
done
"#;
        McpServerConfig {
            command: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), script.to_string()],
            env: Default::default(),
            shared: false,
            transport: None,
            url: None,
            headers: std::collections::HashMap::new(),
            oauth: None,
            enabled: None,
            disabled: None,
            timeout_secs: None,
        }
    }

    #[tokio::test]
    async fn connect_fails_fast_when_server_exits_before_initialize() {
        // A server that prints to stderr and exits before answering
        // `initialize` must fail connect promptly, even with a huge
        // `timeout_secs` (previously the pending request waited it out).
        let config = McpServerConfig {
            command: "/bin/sh".to_string(),
            args: vec!["-c".to_string(), "echo boom >&2; exit 1".to_string()],
            timeout_secs: Some(86_400),
            ..fake_server_config()
        };
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            McpClient::connect("dead".to_string(), &config),
        )
        .await
        .expect("connect must not hang on a server that exited");
        let err = format!("{:#}", result.err().expect("connect must fail"));
        assert!(err.contains("exited"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn connect_in_dir_sets_subprocess_cwd() {
        // Issue #557: owned MCP servers must run in the session project dir.
        let dir = tempfile::tempdir().expect("tempdir");
        let expected = dir.path().canonicalize().expect("canonicalize");

        let client = McpClient::connect_in_dir(
            "cwd-test".to_string(),
            &fake_server_config(),
            Some(dir.path()),
        )
        .await
        .expect("connect");

        let reported = client.server_info().expect("server info").name;
        assert_eq!(
            std::path::Path::new(&reported)
                .canonicalize()
                .expect("canonicalize reported"),
            expected
        );
    }

    #[tokio::test]
    async fn connect_in_dir_missing_dir_falls_back_to_inherited_cwd() {
        let client = McpClient::connect_in_dir(
            "cwd-fallback-test".to_string(),
            &fake_server_config(),
            Some(std::path::Path::new("/nonexistent/jcode-557")),
        )
        .await
        .expect("connect should fall back to inherited cwd");

        let reported = client.server_info().expect("server info").name;
        assert!(!reported.is_empty());
    }
}
