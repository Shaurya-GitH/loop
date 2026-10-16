//! Run Loop as a streamable-HTTP MCP server.

use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;

use axum::extract::Request;
use axum::http::StatusCode;
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use tokio_util::sync::CancellationToken;

use loop_agent::harness::mcp::LoopToolProvider;
use loop_mcp::server::McpServer;

use loop_app_core::Runtime;

/// Bearer token auth middleware. If `expected_token` is `Some`, every request
/// must carry a matching `Authorization: Bearer <token>` header.
async fn bearer_auth(expected: Arc<Option<String>>, req: Request, next: Next) -> Response {
    if let Some(token) = expected.as_ref() {
        let auth_header = req
            .headers()
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());

        match auth_header {
            Some(value) if value == format!("Bearer {token}") => {}
            _ => {
                return (
                    StatusCode::UNAUTHORIZED,
                    "Unauthorized: invalid or missing Bearer token",
                )
                    .into_response();
            }
        }
    }
    next.run(req).await
}

/// Bind address and normalized bearer token for `--serve-mcp`.
#[derive(Debug)]
struct McpListen {
    addr: SocketAddr,
    token: Option<String>,
}

/// Resolve the MCP listen address.
///
/// Loopback binds may omit a token. Any other address, including `0.0.0.0` and
/// `::`, requires a non-empty bearer token.
fn resolve_mcp_listen(host: IpAddr, port: u16, token: Option<String>) -> anyhow::Result<McpListen> {
    let token = token.and_then(|raw| {
        let trimmed = raw.trim().to_string();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        }
    });
    if !host.is_loopback() && token.is_none() {
        anyhow::bail!(
            "refusing to listen on {host}: pass --mcp-token to bind a non-loopback address"
        );
    }
    Ok(McpListen {
        addr: SocketAddr::from((host, port)),
        token,
    })
}

/// Start the MCP HTTP server and block until shutdown.
pub async fn run_mcp_server(
    runtime: Runtime,
    host: IpAddr,
    port: u16,
    token: Option<String>,
) -> anyhow::Result<()> {
    let listen = resolve_mcp_listen(host, port, token)?;
    let harness = Arc::clone(&runtime.harness);
    let gate = Arc::clone(&runtime.tool_gate);
    let ct = CancellationToken::new();

    let service = StreamableHttpService::new(
        move || {
            let tools = harness.tools_snapshot();
            // Calls from MCP clients pass the same checks as the agent's.
            let provider = Arc::new(
                LoopToolProvider::new(tools)
                    .with_tool_hooks(Some(gate.before_hook()), Some(gate.after_hook())),
            );
            Ok(McpServer::new(provider))
        },
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default().with_cancellation_token(ct.child_token()),
    );

    let addr = listen.addr;
    let auth_enabled = listen.token.is_some();
    let expected_token = Arc::new(listen.token);
    let router = axum::Router::new()
        .nest_service("/mcp", service)
        .layer(middleware::from_fn(move |req, next| {
            let expected = Arc::clone(&expected_token);
            bearer_auth(expected, req, next)
        }));

    if auth_enabled {
        eprintln!("Loop MCP server listening on http://{addr}/mcp (auth: Bearer token)");
    } else {
        eprintln!("Loop MCP server listening on http://{addr}/mcp (auth: none)");
    }

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            tokio::signal::ctrl_c().await.ok();
            ct.cancel();
        })
        .await?;

    runtime.mcp_client.disconnect_all().await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use super::resolve_mcp_listen;

    fn v4(octets: [u8; 4]) -> IpAddr {
        IpAddr::V4(Ipv4Addr::from(octets))
    }

    #[test]
    fn loopback_without_token_listens() {
        let listen = resolve_mcp_listen(v4([127, 0, 0, 1]), 3100, None).unwrap();
        assert_eq!(listen.addr.to_string(), "127.0.0.1:3100");
        assert!(listen.token.is_none());
    }

    #[test]
    fn loopback_with_token_keeps_auth() {
        let listen = resolve_mcp_listen(v4([127, 0, 0, 1]), 3100, Some("secret".into())).unwrap();
        assert_eq!(listen.addr.to_string(), "127.0.0.1:3100");
        assert_eq!(listen.token.as_deref(), Some("secret"));
    }

    #[test]
    fn wildcard_with_token_listens() {
        let listen = resolve_mcp_listen(v4([0, 0, 0, 0]), 3100, Some("secret".into())).unwrap();
        assert_eq!(listen.addr.to_string(), "0.0.0.0:3100");
        assert_eq!(listen.token.as_deref(), Some("secret"));
    }

    #[test]
    fn wildcard_without_token_is_refused() {
        let err = resolve_mcp_listen(v4([0, 0, 0, 0]), 3100, None).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("0.0.0.0"), "{msg}");
        assert!(msg.contains("--mcp-token"), "{msg}");
    }

    #[test]
    fn blank_token_counts_as_missing() {
        let listen = resolve_mcp_listen(v4([127, 0, 0, 1]), 3100, Some("  ".into())).unwrap();
        assert!(listen.token.is_none());

        let err = resolve_mcp_listen(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 3100, Some("".into()))
            .unwrap_err();
        assert!(format!("{err:#}").contains("::"));
    }

    #[test]
    fn ipv6_loopback_without_token_listens() {
        let listen = resolve_mcp_listen(IpAddr::V6(Ipv6Addr::LOCALHOST), 3100, None).unwrap();
        assert!(listen.addr.ip().is_loopback());
        assert!(listen.token.is_none());
    }
}
