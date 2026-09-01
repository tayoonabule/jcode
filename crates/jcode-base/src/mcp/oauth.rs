//! OAuth 2.0 (PKCE + dynamic client registration) for remote MCP servers.
//!
//! Implements the MCP authorization flow: a 401 from the server points at
//! protected-resource metadata, which points at an authorization server, which
//! we register with dynamically and then drive through a loopback PKCE flow.
//!
//! Memory notes: nothing here is cached in process beyond the small token
//! struct held by the owning transport. Metadata documents are parsed into
//! `Option<String>` fields and dropped immediately.

use super::protocol::McpOAuthConfig;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Persisted credentials for one remote MCP server.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpOAuthTokens {
    pub access_token: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Unix seconds. 0 means "no known expiry".
    #[serde(default)]
    pub expires_at: i64,
    /// Client id issued by dynamic registration, reused across refreshes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_endpoint: Option<String>,
    /// Redirect URI registered with the dynamic OAuth client.
    ///
    /// Dynamic clients are bound to their redirect URI. Reusing a stored
    /// client id with a newly allocated loopback port makes Atlassian reject
    /// the authorization request, often as an opaque internal server error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
}

impl McpOAuthTokens {
    pub fn is_expired(&self) -> bool {
        self.expires_at > 0 && chrono::Utc::now().timestamp() >= self.expires_at - 60
    }
}

/// Credentials live under `$JCODE_HOME` when set (tests and sandboxed runs),
/// otherwise `~/.jcode`, matching the rest of jcode's credential storage.
fn auth_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("JCODE_HOME") {
        return PathBuf::from(home).join("mcp-auth");
    }
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".jcode")
        .join("mcp-auth")
}

/// File name is derived from the server name so several servers on one host
/// keep separate credentials.
fn token_path(server: &str) -> PathBuf {
    let safe: String = server
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    auth_dir().join(format!("{safe}.json"))
}

pub fn load_tokens(server: &str) -> Option<McpOAuthTokens> {
    let data = std::fs::read_to_string(token_path(server)).ok()?;
    serde_json::from_str(&data).ok()
}

pub fn save_tokens(server: &str, tokens: &McpOAuthTokens) -> Result<()> {
    let path = token_path(server);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, serde_json::to_string(tokens)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

pub fn clear_tokens(server: &str) {
    let _ = std::fs::remove_file(token_path(server));
}

#[derive(Debug, Default, Deserialize)]
struct ProtectedResourceMetadata {
    #[serde(default)]
    authorization_servers: Vec<String>,
    /// The resource server may advertise the scopes its authorization request
    /// must include. Atlassian's authv2 endpoint publishes the Jira work
    /// scopes here, while its authorization-server metadata omits
    /// `scopes_supported`. Ignoring this field causes Atlassian to mint the
    /// narrower agent-interface grant, which then fails Jira data tools with
    /// "Unauthorized; scope does not match".
    #[serde(default)]
    scopes_supported: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct AuthServerMetadata {
    #[serde(default)]
    authorization_endpoint: Option<String>,
    #[serde(default)]
    token_endpoint: Option<String>,
    #[serde(default)]
    registration_endpoint: Option<String>,
    /// Scopes the server advertises. Some servers (Semrush) reject or
    /// downgrade requests that ask for no scope at all.
    #[serde(default)]
    scopes_supported: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct RegistrationResponse {
    client_id: String,
}

#[derive(Debug, Deserialize)]
struct TokenResponse {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<i64>,
}

/// Build the `scope` value to request from the advertised list.
///
/// Returns `None` when the server advertises nothing, so servers that reject an
/// explicit scope keep working unchanged.
fn requested_scope(supported: &[String]) -> Option<String> {
    if supported.is_empty() {
        return None;
    }
    // `offline` and `offline_access` are synonyms across providers; asking for
    // both can be rejected, so take whichever this server lists first.
    let mut scopes: Vec<&str> = Vec::new();
    let mut has_offline = false;
    for scope in supported {
        let is_offline = scope == "offline" || scope == "offline_access";
        if is_offline {
            if has_offline {
                continue;
            }
            has_offline = true;
        }
        scopes.push(scope.as_str());
    }
    Some(scopes.join(" "))
}

/// Parse `resource_metadata="..."` out of a `WWW-Authenticate` header.
pub fn resource_metadata_from_challenge(header: &str) -> Option<String> {
    let idx = header.find("resource_metadata=")?;
    let rest = &header[idx + "resource_metadata=".len()..];
    let rest = rest.trim_start_matches('"');
    let end = rest.find('"').unwrap_or(rest.len());
    let value = rest[..end].trim_end_matches(',').trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

fn well_known(base: &url::Url, suffix: &str) -> String {
    let mut u = base.clone();
    // OAuth issuers are allowed to include a tenant or authorization-server
    // path. Atlassian uses exactly that shape, for example
    // `/VCeDsk8ZHncYF1g234fKtc4lNipbBhu3`. Replacing the path with the
    // well-known suffix silently queries the provider root and makes a valid
    // server look like it has no dynamic registration support.
    let base_path = u.path().trim_end_matches('/');
    let suffix = suffix.trim_start_matches('/');
    let path = if base_path.is_empty() {
        format!("/{suffix}")
    } else {
        format!("{base_path}/{suffix}")
    };
    u.set_path(&path);
    u.set_query(None);
    u.to_string()
}

async fn fetch_json<T: serde::de::DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
) -> Option<T> {
    let resp = client.get(url).send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<T>().await.ok()
}

/// Discover the authorization endpoints for a server URL, optionally guided by
/// the `WWW-Authenticate` challenge the server returned.
async fn discover(
    client: &reqwest::Client,
    server_url: &str,
    challenge: Option<&str>,
) -> Result<AuthServerMetadata> {
    let base = url::Url::parse(server_url).context("Invalid MCP server URL")?;

    let resource_metadata_url = challenge
        .and_then(resource_metadata_from_challenge)
        .unwrap_or_else(|| well_known(&base, "/.well-known/oauth-protected-resource"));

    let resource_meta = fetch_json::<ProtectedResourceMetadata>(client, &resource_metadata_url)
        .await
        .unwrap_or_default();
    let issuer = resource_meta
        .authorization_servers
        .into_iter()
        .next()
        .unwrap_or_else(|| base.origin().ascii_serialization());
    let resource_scopes = resource_meta.scopes_supported.clone();

    let issuer_url = url::Url::parse(&issuer).unwrap_or(base.clone());

    for candidate in [
        well_known(&issuer_url, "/.well-known/oauth-authorization-server"),
        well_known(&issuer_url, "/.well-known/openid-configuration"),
    ] {
        if let Some(meta) = fetch_json::<AuthServerMetadata>(client, &candidate).await
            && meta.authorization_endpoint.is_some()
            && meta.token_endpoint.is_some()
        {
            return Ok(AuthServerMetadata {
                scopes_supported: if meta.scopes_supported.is_empty() {
                    resource_scopes.clone()
                } else {
                    meta.scopes_supported
                },
                ..meta
            });
        }
    }

    // Fall back to the conventional endpoint layout.
    Ok(AuthServerMetadata {
        authorization_endpoint: Some(well_known(&issuer_url, "/authorize")),
        token_endpoint: Some(well_known(&issuer_url, "/token")),
        registration_endpoint: Some(well_known(&issuer_url, "/register")),
        scopes_supported: resource_scopes,
    })
}

/// Run the full browser-based authorization code flow for a remote MCP server.
pub async fn authorize(
    server_name: &str,
    server_url: &str,
    challenge_header: Option<&str>,
    oauth_config: Option<&McpOAuthConfig>,
    no_browser: bool,
) -> Result<McpOAuthTokens> {
    authorize_with_opener(
        server_name,
        server_url,
        challenge_header,
        oauth_config,
        move |url| {
            // Test hook: fetch the authorization URL instead of handing it to a
            // real browser, so automated runs can exercise the whole flow.
            if std::env::var_os("JCODE_MCP_AUTH_AUTOFOLLOW").is_some() {
                let url = url.to_string();
                tokio::spawn(async move {
                    let _ = reqwest::get(&url).await;
                });
                return;
            }
            if !no_browser {
                // Do not wait on the browser process. On macOS, `open::that`
                // can inherit the MCP transport's lifetime and leave the
                // authorization flow looking stalled even though no visible
                // prompt was opened. The detached variant returns immediately
                // and lets the system browser own the OAuth tab.
                let _ = open::that_detached(url);
            }
        },
    )
    .await
}

/// Same as [`authorize`], but the caller decides how the authorization URL is
/// presented. Tests substitute a fake user-agent for the system browser, which
/// lets the whole 401 -> discovery -> registration -> token flow be exercised.
pub async fn authorize_with_opener<F>(
    server_name: &str,
    server_url: &str,
    challenge_header: Option<&str>,
    oauth_config: Option<&McpOAuthConfig>,
    open_url: F,
) -> Result<McpOAuthTokens>
where
    F: FnOnce(&str) + Send + 'static,
{
    let client = super::http::shared_client();
    let meta = discover(&client, server_url, challenge_header).await?;
    let authorization_endpoint = oauth_config
        .and_then(|config| config.authorization_endpoint.clone())
        .or(meta.authorization_endpoint)
        .context("Authorization server did not advertise an authorization_endpoint")?;
    let token_endpoint = oauth_config
        .and_then(|config| config.token_endpoint.clone())
        .or(meta.token_endpoint)
        .context("Authorization server did not advertise a token_endpoint")?;

    // A configured scope list is required for providers such as Google that do
    // not expose scopes through MCP metadata. Otherwise request only the scopes
    // the authorization server advertises.
    let scope = oauth_config
        .filter(|config| !config.scopes.is_empty())
        .map(|config| config.scopes.join(" "))
        .or_else(|| requested_scope(&meta.scopes_supported));

    // Bind the callback before opening the browser. A fixed URI is useful for
    // OAuth web clients that require an exact redirect registration.
    let saved_redirect_uri = load_tokens(server_name).and_then(|tokens| tokens.redirect_uri);
    let (listener, redirect_uri) = if let Some(configured) = oauth_config
        .and_then(|config| config.redirect_uri.as_deref())
        .or(saved_redirect_uri.as_deref())
    {
        let redirect = url::Url::parse(configured).context("Invalid OAuth redirect_uri")?;
        if redirect.scheme() != "http"
            || !matches!(redirect.host_str(), Some("127.0.0.1" | "localhost"))
            || redirect.query().is_some()
            || redirect.fragment().is_some()
        {
            anyhow::bail!("OAuth redirect_uri must be a query-free loopback HTTP URL");
        }
        let host = redirect.host_str().unwrap_or("127.0.0.1");
        let port = redirect.port().context("OAuth redirect_uri must include a port")?;
        let listener = tokio::net::TcpListener::bind((host, port))
            .await
            .context("Failed to bind configured OAuth callback listener")?;
        (listener, configured.to_string())
    } else {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .context("Failed to bind OAuth callback listener")?;
        let port = listener.local_addr()?.port();
        (listener, format!("http://127.0.0.1:{port}/callback"))
    };

    let client_id = oauth_config
        .and_then(|config| config.client_id.clone())
        // A dynamically registered client is bound to the redirect URI used
        // during registration. Only reuse it when we also persisted that URI.
        .or_else(|| {
            load_tokens(server_name).and_then(|t| {
                t.redirect_uri
                    .filter(|registered| registered == &redirect_uri)
                    .and(t.client_id)
            })
        })
        .unwrap_or_else(|| {
            // This sentinel is replaced below for dynamic registration. Keeping
            // the branch explicit avoids accidentally registering when a static
            // provider configuration is incomplete.
            String::new()
        });
    let client_secret = oauth_config.and_then(|config| config.client_secret.clone());
    let client_id = if !client_id.is_empty() {
        client_id
    } else {
        let registration_endpoint = meta
            .registration_endpoint
            .context("Server requires OAuth but supports no dynamic client registration")?;
        let body = serde_json::json!({
            "client_name": "jcode",
            "redirect_uris": [redirect_uri],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
            "scope": scope.clone().unwrap_or_default(),
        });
        let resp = client
            .post(&registration_endpoint)
            .json(&body)
            .send()
            .await
            .context("Dynamic client registration failed")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let detail = resp.text().await.unwrap_or_default();
            if status == reqwest::StatusCode::FORBIDDEN {
                anyhow::bail!(
                    "Dynamic client registration rejected (403): the OAuth provider does not allow this MCP client; Figma remote MCP currently requires an approved client integration"
                );
            }
            anyhow::bail!(
                "Dynamic client registration rejected ({}): {}",
                status.as_u16(),
                detail.chars().take(200).collect::<String>()
            );
        }
        resp.json::<RegistrationResponse>().await?.client_id
    };

    let (verifier, code_challenge) = crate::auth::oauth::generate_pkce_public();
    let state = crate::auth::oauth::generate_state_public();

    let mut auth_url = url::Url::parse(&authorization_endpoint)?;
    auth_url
        .query_pairs_mut()
        .append_pair("response_type", "code")
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("code_challenge", &code_challenge)
        .append_pair("code_challenge_method", "S256")
        .append_pair("state", &state)
        .append_pair("resource", server_url);
    // Google access tokens are short-lived. Request offline access so the
    // first interactive login also gives us a refresh token instead of
    // reopening a browser every hour. Providers that ignore these standard
    // parameters simply continue to receive the normal authorization request.
    auth_url
        .query_pairs_mut()
        .append_pair("access_type", "offline");
    if load_tokens(server_name)
        .as_ref()
        .is_some_and(|tokens| tokens.refresh_token.is_none())
    {
        // An existing grant may otherwise omit a refresh token. This is only
        // reached when re-authorizing an already-expired, non-refreshable
        // credential, so consent is not part of the normal request path.
        auth_url
            .query_pairs_mut()
            .append_pair("prompt", "consent");
    }
    if let Some(scope) = &scope {
        auth_url.query_pairs_mut().append_pair("scope", scope);
    }

    let auth_url = auth_url.to_string();
    crate::terminal_eprintln!(
        "\nAuthorize the MCP server '{server_name}' in your browser. If it did not open, visit:\n{auth_url}\n"
    );
    // Hand the URL over only after the listener is bound, so a fast responder
    // (a test's fake browser) cannot beat us to the callback.
    open_url(&auth_url);

    let code = tokio::time::timeout(
        std::time::Duration::from_secs(300),
        crate::auth::oauth::wait_for_callback_async_on_listener(listener, &state),
    )
    .await
    .context("Timed out waiting for MCP OAuth callback")??;

    let mut params = vec![
        ("grant_type", "authorization_code"),
        ("code", code.as_str()),
        ("redirect_uri", redirect_uri.as_str()),
        ("client_id", client_id.as_str()),
        ("code_verifier", verifier.as_str()),
        ("resource", server_url),
    ];
    if let Some(scope) = scope.as_deref() {
        params.push(("scope", scope));
    }
    if let Some(secret) = client_secret.as_deref() {
        params.push(("client_secret", secret));
    }
    let resp = client
        .post(&token_endpoint)
        .form(&params)
        .send()
        .await
        .context("Token exchange request failed")?;
    if !resp.status().is_success() {
        anyhow::bail!("Token exchange failed ({})", resp.status().as_u16());
    }
    let token: TokenResponse = resp.json().await?;

    let tokens = McpOAuthTokens {
        access_token: token.access_token,
        refresh_token: token.refresh_token,
        expires_at: token
            .expires_in
            .map(|secs| chrono::Utc::now().timestamp() + secs)
            .unwrap_or(0),
        client_id: Some(client_id),
        token_endpoint: Some(token_endpoint),
        redirect_uri: Some(redirect_uri),
    };
    save_tokens(server_name, &tokens)?;
    Ok(tokens)
}

/// Refresh an access token in place. Returns `None` when refresh is impossible
/// so the caller can fall back to a full browser flow.
pub async fn refresh(
    server_name: &str,
    tokens: &McpOAuthTokens,
    oauth_config: Option<&McpOAuthConfig>,
) -> Option<McpOAuthTokens> {
    let refresh_token = tokens.refresh_token.as_deref()?;
    let token_endpoint = tokens.token_endpoint.as_deref()?;
    let client_id = tokens.client_id.as_deref()?;

    let mut params = vec![
        ("grant_type", "refresh_token"),
        ("refresh_token", refresh_token),
        ("client_id", client_id),
    ];
    if let Some(secret) = oauth_config.and_then(|config| config.client_secret.as_deref()) {
        params.push(("client_secret", secret));
    }
    let resp = super::http::shared_client()
        .post(token_endpoint)
        .form(&params)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let token: TokenResponse = resp.json().await.ok()?;
    let refreshed = McpOAuthTokens {
        access_token: token.access_token,
        refresh_token: token.refresh_token.or_else(|| tokens.refresh_token.clone()),
        expires_at: token
            .expires_in
            .map(|secs| chrono::Utc::now().timestamp() + secs)
            .unwrap_or(0),
        client_id: tokens.client_id.clone(),
        token_endpoint: tokens.token_endpoint.clone(),
        redirect_uri: tokens.redirect_uri.clone(),
    };
    let _ = save_tokens(server_name, &refreshed);
    Some(refreshed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_resource_metadata_from_challenge() {
        let header =
            r#"Bearer error="invalid_token", resource_metadata="https://mcp.granola.ai/.well-known/oauth-protected-resource""#;
        assert_eq!(
            resource_metadata_from_challenge(header).as_deref(),
            Some("https://mcp.granola.ai/.well-known/oauth-protected-resource")
        );
    }

    #[test]
    fn challenge_without_metadata_yields_none() {
        assert!(resource_metadata_from_challenge("Bearer realm=\"mcp\"").is_none());
    }

    #[test]
    fn well_known_preserves_issuer_path() {
        let issuer = url::Url::parse(
            "https://auth.atlassian.com/VCeDsk8ZHncYF1g234fKtc4lNipbBhu3",
        )
        .unwrap();
        assert_eq!(
            well_known(&issuer, "/.well-known/oauth-authorization-server"),
            "https://auth.atlassian.com/VCeDsk8ZHncYF1g234fKtc4lNipbBhu3/.well-known/oauth-authorization-server"
        );
    }

    #[test]
    fn no_advertised_scopes_means_no_scope_parameter() {
        // Granola advertises "mcp" but some servers advertise nothing; sending
        // an empty scope can be rejected, so omit it entirely.
        assert_eq!(requested_scope(&[]), None);
    }

    #[test]
    fn advertised_scopes_are_requested_verbatim() {
        let scopes = ["mcp.access".to_string()];
        assert_eq!(requested_scope(&scopes).as_deref(), Some("mcp.access"));
    }

    #[test]
    fn protected_resource_scopes_can_supply_missing_authorization_scopes() {
        let metadata: ProtectedResourceMetadata = serde_json::from_value(serde_json::json!({
            "authorization_servers": ["https://auth.example/issuer"],
            "scopes_supported": ["read:jira-work", "search:jira-work"]
        }))
        .unwrap();
        assert_eq!(
            requested_scope(&metadata.scopes_supported).as_deref(),
            Some("read:jira-work search:jira-work")
        );
    }

    #[test]
    fn duplicate_offline_synonyms_are_collapsed() {
        // Semrush advertises both `offline` and `offline_access`; requesting
        // both can be rejected as an unknown scope combination.
        let scopes = [
            "offline_access".to_string(),
            "offline".to_string(),
            "mcp.access".to_string(),
        ];
        let requested = requested_scope(&scopes).expect("scope");
        assert_eq!(requested, "offline_access mcp.access");
        assert!(
            !requested.split(' ').any(|s| s == "offline"),
            "only one offline synonym should be requested: {requested}"
        );
    }

    #[test]
    fn expiry_uses_a_leading_skew() {
        let mut tokens = McpOAuthTokens {
            access_token: "a".into(),
            refresh_token: None,
            expires_at: 0,
            client_id: None,
            token_endpoint: None,
            redirect_uri: None,
        };
        assert!(!tokens.is_expired(), "0 means unknown expiry, not expired");
        tokens.expires_at = chrono::Utc::now().timestamp() + 10;
        assert!(tokens.is_expired(), "within the 60s skew window");
        tokens.expires_at = chrono::Utc::now().timestamp() + 600;
        assert!(!tokens.is_expired());
    }
}
