//! MCP client manager: connect to external MCP servers via stdio or streamable HTTP.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use http::{HeaderName, HeaderValue};
use rmcp::service::RunningService;
use rmcp::transport::auth::AuthClient;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::{ConfigureCommandExt, StreamableHttpClientTransport, TokioChildProcess};
use rmcp::{RoleClient, ServiceExt};
use tokio::sync::RwLock;

use crate::oauth;
pub use crate::oauth::OAuthLoginOutcome;

/// Transport configuration for connecting to an MCP server.
#[derive(Debug, Clone)]
pub enum McpTransport {
    /// Spawn a local child process and communicate over stdin/stdout.
    Stdio {
        command: String,
        args: Vec<String>,
        env: HashMap<String, String>,
    },
    /// Connect to a remote MCP server over streamable HTTP.
    Http {
        url: String,
        headers: HashMap<String, String>,
    },
}

/// Configuration for an external MCP server to connect to.
#[derive(Debug, Clone)]
pub struct McpServerEntry {
    /// Human-readable name (used as key and tool prefix).
    pub name: String,
    /// How to reach the server.
    pub transport: McpTransport,
}

/// A live connection to an MCP server.
pub struct McpConnection {
    /// Server name.
    pub name: String,
    /// The running rmcp client service.
    pub client: RunningService<RoleClient, ()>,
    /// Tools discovered from this server.
    pub tools: Vec<rmcp::model::Tool>,
}

/// Manages connections to multiple external MCP servers.
pub struct McpClientManager {
    connections: Arc<RwLock<HashMap<String, McpConnection>>>,
    /// Directory of per-server OAuth credential files. Absent in tests and
    /// harnesses that do not log in.
    oauth_dir: Option<PathBuf>,
}

impl McpClientManager {
    /// Create an empty manager with no OAuth credential directory.
    pub fn new() -> Self {
        Self {
            connections: Arc::new(RwLock::new(HashMap::new())),
            oauth_dir: None,
        }
    }

    /// Create an empty manager that stores OAuth tokens under `dir`.
    pub fn with_oauth_dir(dir: impl Into<PathBuf>) -> Self {
        Self {
            connections: Arc::new(RwLock::new(HashMap::new())),
            oauth_dir: Some(dir.into()),
        }
    }

    /// Path where this server's OAuth tokens are stored.
    ///
    /// `Ok(None)` when the manager has no credential directory.
    pub fn oauth_credential_path(&self, name: &str) -> Result<Option<PathBuf>, String> {
        let Some(dir) = &self.oauth_dir else {
            return Ok(None);
        };
        oauth::credential_path(dir, name).map(Some)
    }

    /// Open a browser OAuth flow for a remote server and save the tokens.
    ///
    /// `notify_url` receives the authorization URL. The caller opens it.
    pub async fn oauth_login<F>(
        &self,
        name: &str,
        url: &str,
        notify_url: F,
    ) -> Result<OAuthLoginOutcome, String>
    where
        F: FnOnce(&str) + Send,
    {
        let Some(path) = self.oauth_credential_path(name)? else {
            return Err("oauth credential storage is not configured".into());
        };
        oauth::login(url, &path, notify_url).await
    }

    /// Delete saved OAuth tokens for `name`. Returns whether a file was removed.
    pub async fn clear_oauth_credentials(&self, name: &str) -> Result<bool, String> {
        let Some(path) = self.oauth_credential_path(name)? else {
            return Ok(false);
        };
        oauth::clear_credentials(&path).await
    }

    /// Connect to a single MCP server. Returns the tool count on success.
    pub async fn connect(&self, entry: &McpServerEntry) -> Result<usize, String> {
        let client = match &entry.transport {
            McpTransport::Stdio { command, args, env } => {
                let args = args.clone();
                let envs: Vec<(String, String)> =
                    env.iter().map(|(k, v)| (k.clone(), v.clone())).collect();

                let cmd = tokio::process::Command::new(command).configure(|cmd| {
                    for arg in &args {
                        cmd.arg(arg);
                    }
                    for (k, v) in &envs {
                        cmd.env(k, v);
                    }
                });

                let transport = TokioChildProcess::new(cmd)
                    .map_err(|e| format!("failed to spawn MCP server '{}': {e}", entry.name))?;

                ().serve(transport).await.map_err(|e| {
                    format!(
                        "failed to initialize MCP session with '{}': {e}",
                        entry.name
                    )
                })?
            }
            McpTransport::Http { url, headers } => {
                self.connect_http(&entry.name, url, headers).await?
            }
        };

        self.store_connection(entry.name.clone(), client).await
    }

    async fn connect_http(
        &self,
        name: &str,
        url: &str,
        headers: &HashMap<String, String>,
    ) -> Result<RunningService<RoleClient, ()>, String> {
        let (bearer, custom_headers) = split_http_headers(headers)?;
        let store_path = self.oauth_credential_path(name)?;
        if let Some(path) = store_path.as_deref().filter(|path| path.exists()) {
            if let Some(manager) = oauth::authorized_manager(url, path).await? {
                let transport = StreamableHttpClientTransport::with_client(
                    AuthClient::new(mcp_http_client(), manager),
                    http_config(url, custom_headers.clone(), None),
                );
                return serve_http(name, url, transport).await;
            }
        }

        let transport = StreamableHttpClientTransport::with_client(
            mcp_http_client(),
            http_config(url, custom_headers, bearer),
        );
        serve_http(name, url, transport).await
    }

    async fn store_connection(
        &self,
        name: String,
        client: RunningService<RoleClient, ()>,
    ) -> Result<usize, String> {
        let tools_result = client
            .list_tools(None)
            .await
            .map_err(|e| format!("failed to list tools from '{name}': {e}"))?;

        let tools = tools_result.tools;
        let count = tools.len();
        self.connections.write().await.insert(
            name.clone(),
            McpConnection {
                name,
                client,
                tools,
            },
        );
        Ok(count)
    }

    /// Connect to all entries, logging errors but not failing the whole batch.
    pub async fn connect_all(
        &self,
        entries: &[McpServerEntry],
    ) -> Vec<(String, Result<usize, String>)> {
        let mut results = Vec::new();
        for entry in entries {
            let result = self.connect(entry).await;
            results.push((entry.name.clone(), result));
        }
        results
    }

    /// Disconnect a single server by name. Returns true if it was connected.
    pub async fn disconnect(&self, name: &str) -> bool {
        if let Some(conn) = self.connections.write().await.remove(name) {
            let _ = conn.client.cancel().await;
            true
        } else {
            false
        }
    }

    /// Disconnect all servers.
    pub async fn disconnect_all(&self) {
        let mut conns = self.connections.write().await;
        for (_, conn) in conns.drain() {
            let _ = conn.client.cancel().await;
        }
    }

    /// List connected server names and their tool counts.
    pub async fn list_connections(&self) -> Vec<(String, usize)> {
        self.connections
            .read()
            .await
            .iter()
            .map(|(name, conn)| (name.clone(), conn.tools.len()))
            .collect()
    }

    /// Connected servers and the tool names each one advertised, sorted by name.
    pub async fn list_connection_tools(&self) -> Vec<(String, Vec<String>)> {
        let mut listed: Vec<(String, Vec<String>)> = self
            .connections
            .read()
            .await
            .iter()
            .map(|(name, conn)| {
                let mut tools: Vec<String> = conn
                    .tools
                    .iter()
                    .map(|tool| tool.name.to_string())
                    .collect();
                tools.sort();
                (name.clone(), tools)
            })
            .collect();
        listed.sort_by(|a, b| a.0.cmp(&b.0));
        listed
    }

    /// Whether `server` marks its tool `tool` read-only (`readOnlyHint`).
    pub async fn read_only_hint(&self, server: &str, tool: &str) -> bool {
        self.connections
            .read()
            .await
            .get(server)
            .is_some_and(|conn| {
                conn.tools.iter().any(|t| {
                    t.name == tool
                        && t.annotations.as_ref().and_then(|a| a.read_only_hint) == Some(true)
                })
            })
    }

    /// Get a read lock on connections (for bridge tool generation).
    pub fn connections(&self) -> &Arc<RwLock<HashMap<String, McpConnection>>> {
        &self.connections
    }
}

impl Default for McpClientManager {
    fn default() -> Self {
        Self::new()
    }
}

fn auth_required_message(name: &str) -> String {
    format!("authorization required for mcp server '{name}'; run /mcp login {name}")
}

fn mcp_http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .pool_max_idle_per_host(0)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .expect("failed to build mcp http client")
}

fn http_config(
    url: &str,
    custom_headers: HashMap<HeaderName, HeaderValue>,
    bearer: Option<String>,
) -> StreamableHttpClientTransportConfig {
    let mut config =
        StreamableHttpClientTransportConfig::with_uri(url).custom_headers(custom_headers);
    if let Some(token) = bearer {
        config = config.auth_header(token);
    }
    config
}

fn split_http_headers(
    headers: &HashMap<String, String>,
) -> Result<(Option<String>, HashMap<HeaderName, HeaderValue>), String> {
    let mut bearer = None;
    let mut custom = HashMap::new();
    for (name, value) in headers {
        if name.eq_ignore_ascii_case("authorization") {
            let token = value
                .trim()
                .strip_prefix("Bearer ")
                .or_else(|| value.trim().strip_prefix("bearer "))
                .unwrap_or(value.trim());
            if token.is_empty() {
                return Err("authorization header is empty".into());
            }
            bearer = Some(token.to_string());
            continue;
        }
        let header_name = HeaderName::try_from(name.as_str())
            .map_err(|e| format!("invalid header name '{name}': {e}"))?;
        let header_value = HeaderValue::from_str(value)
            .map_err(|e| format!("invalid header value for '{name}': {e}"))?;
        custom.insert(header_name, header_value);
    }
    Ok((bearer, custom))
}

async fn serve_http<C>(
    name: &str,
    url: &str,
    transport: StreamableHttpClientTransport<C>,
) -> Result<RunningService<RoleClient, ()>, String>
where
    C: rmcp::transport::streamable_http_client::StreamableHttpClient,
{
    ().serve(transport).await.map_err(|err| {
        if err.is_authorization_required() {
            auth_required_message(name)
        } else {
            format!("failed to connect to MCP server '{name}' at {url}: {err}")
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn authorization_header_becomes_a_bearer_token() {
        let headers = HashMap::from([
            ("Authorization".into(), "Bearer ntn_secret".into()),
            ("X-Trace".into(), "loop".into()),
        ]);
        let (bearer, custom) = split_http_headers(&headers).unwrap();
        assert_eq!(bearer.as_deref(), Some("ntn_secret"));
        assert_eq!(
            custom.get(&HeaderName::from_static("x-trace")).unwrap(),
            "loop"
        );
    }

    #[tokio::test]
    async fn http_401_asks_for_mcp_login() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = axum::Router::new().route(
            "/mcp",
            axum::routing::post(|| async {
                (
                    axum::http::StatusCode::UNAUTHORIZED,
                    [(axum::http::header::WWW_AUTHENTICATE, "Bearer realm=\"mcp\"")],
                    "",
                )
            }),
        );
        let server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let manager = McpClientManager::new();
        let err = manager
            .connect(&McpServerEntry {
                name: "notion".into(),
                transport: McpTransport::Http {
                    url: format!("http://{addr}/mcp"),
                    headers: HashMap::new(),
                },
            })
            .await
            .unwrap_err();
        server.abort();
        assert!(
            err.contains("run /mcp login notion"),
            "unexpected error: {err}"
        );
    }
}
