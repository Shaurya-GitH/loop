//! First-class web tools: `web_fetch` and `web_search`.
//!
//! Both tools run in the harness process (host network), not inside the
//! sandbox environment, and every outbound URL passes the [`guard`] SSRF
//! checks: http/https only, no URL credentials, and connections only to
//! public addresses (redirects included). Redirects are followed manually so
//! each hop is re-validated; downloads are capped and truncated before they
//! reach the model context.
//!
//! - `web_fetch` goes through Soket's fetch API when a Soket key is available
//!   (the page is fetched on Soket's side and returned as markdown) and
//!   otherwise fetches directly. `webFetch.provider` pins either path;
//!   `webFetch.allowPrivateAddresses` opts a deployment into intranet mirrors
//!   (default: fail closed) and implies the direct path.
//! - `web_search` uses the `webSearch.provider` backend (Soket by default);
//!   its API key comes from `/login` (auth.json), `apiKeyEnv` env vars, or the
//!   backend's default env vars. See [`search`].

mod guard;
mod html;
mod search;

pub use search::{
    create_web_search_tool, verify_web_search_key, web_search_backend, WebSearchBackendInfo,
    WebSearchConfig, DEFAULT_WEB_SEARCH_PROVIDER, WEB_SEARCH_BACKENDS,
};

use std::sync::OnceLock;
use std::time::Duration;

use reqwest::header::LOCATION;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::json;
use tokio_util::sync::CancellationToken;

use crate::harness::utils::{truncate_head, DEFAULT_MAX_BYTES};
use crate::types::{AgentTool, AgentToolResult};
use guard::{validate_request_url, FetchPolicy};
use loop_ai::providers::{SOKET_API_KEY_ENVS, SOKET_BASE_URL};
use loop_ai::{TextContent, ToolResultContent};

/// Hard cap on bytes downloaded per page (the model sees [`DEFAULT_MAX_BYTES`]).
const MAX_DOWNLOAD_BYTES: usize = 2 * 1024 * 1024;
/// Connection establishment cap per request hop.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Overall cap per request hop (redirects each get a fresh budget, bounded by hop count).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
/// Maximum followed redirects, each re-validated.
const MAX_REDIRECTS: usize = 5;

/// Where `web_fetch` sends its requests (`webFetch.provider`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum WebFetchProvider {
    /// Soket when a Soket key is available (and private addresses are not
    /// allowed), otherwise direct.
    #[default]
    Auto,
    /// Always Soket's fetch API; tool creation fails without a key.
    Soket,
    /// Always fetch from this machine.
    Direct,
}

/// Configuration for the `web_fetch` tool (`webFetch` in settings.json).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct WebFetchConfig {
    /// Allow loopback, private, link-local and reserved addresses. Intended
    /// for intranet mirrors; the default fails closed. Soket cannot reach an
    /// intranet, so in `auto` mode this selects the direct path.
    #[serde(default)]
    pub allow_private_addresses: bool,
    /// Soket fetch API or direct fetch.
    #[serde(default, skip_serializing_if = "is_auto")]
    pub provider: WebFetchProvider,
}

fn is_auto(provider: &WebFetchProvider) -> bool {
    *provider == WebFetchProvider::Auto
}

/// Soket's search/fetch API access: the key stored for the Soket model
/// provider and the API base URL.
#[derive(Clone)]
pub struct SoketWebApi {
    /// API base URL, including the version path.
    pub base_url: String,
    /// Bearer token.
    pub api_key: String,
}

impl std::fmt::Debug for SoketWebApi {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SoketWebApi")
            .field("base_url", &self.base_url)
            .finish_non_exhaustive()
    }
}

impl SoketWebApi {
    /// Access to the production Soket API with `api_key`.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            base_url: SOKET_BASE_URL.into(),
            api_key: api_key.into(),
        }
    }

    /// Resolve the Soket key: the stored `/login soket` credential, then the
    /// `SOKET_API_KEY` family of env vars. `None` when neither is set.
    pub fn resolve(stored_key: Option<&str>) -> Option<Self> {
        let non_empty = |v: &str| !v.trim().is_empty();
        stored_key
            .filter(|k| non_empty(k))
            .map(str::to_string)
            .or_else(|| {
                SOKET_API_KEY_ENVS
                    .iter()
                    .find_map(|name| std::env::var(name).ok().filter(|v| non_empty(v)))
            })
            .map(Self::new)
    }
}

/// How a built `web_fetch` tool reaches pages.
#[derive(Clone)]
enum FetchRoute {
    Direct,
    Soket { endpoint: Url, api_key: String },
}

impl WebFetchConfig {
    fn policy(self) -> FetchPolicy {
        FetchPolicy {
            allow_private_addresses: self.allow_private_addresses,
        }
    }
}

/// One pooled client per policy, shared for the whole process.
fn shared_client(policy: FetchPolicy) -> Result<reqwest::Client, String> {
    static STRICT: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    static PRIVATE_HOSTS_ALLOWED: OnceLock<Option<reqwest::Client>> = OnceLock::new();
    let cell = if policy.allow_private_addresses {
        &PRIVATE_HOSTS_ALLOWED
    } else {
        &STRICT
    };
    cell.get_or_init(|| build_client(policy))
        .clone()
        .ok_or_else(|| "HTTP client init failed".to_string())
}

fn build_client(policy: FetchPolicy) -> Option<reqwest::Client> {
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        // Redirects are followed (and re-validated) by `fetch_url`.
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(concat!("loop/", env!("CARGO_PKG_VERSION")))
        .dns_resolver(std::sync::Arc::new(guard::GuardResolver::new(policy)))
        .build();
    if let Err(error) = &client {
        tracing::error!("web tools: HTTP client init failed: {error}");
    }
    client.ok()
}

/// Build the `web_fetch` tool. `soket` (when resolved) routes fetches through
/// Soket's fetch API per [`WebFetchProvider`]. `Err` covers HTTP client init
/// failure, a bad Soket base URL, and `provider: soket` without a key.
pub fn create_web_fetch_tool(
    config: WebFetchConfig,
    soket: Option<SoketWebApi>,
) -> Result<AgentTool, String> {
    let policy = config.policy();
    let client = shared_client(policy)?;
    let soket = match (config.provider, soket) {
        (WebFetchProvider::Direct, _) => None,
        (WebFetchProvider::Auto, _) if config.allow_private_addresses => None,
        (WebFetchProvider::Soket, None) => {
            return Err(
                "webFetch.provider is \"soket\" but no Soket API key: run /login soket or set SOKET_API_KEY"
                    .into(),
            )
        }
        (_, soket) => soket,
    };
    let route = match soket {
        None => FetchRoute::Direct,
        Some(api) => FetchRoute::Soket {
            endpoint: search::endpoint(&search::resolve_base_url(&api.base_url, policy)?, "fetch"),
            api_key: api.api_key,
        },
    };
    Ok(AgentTool::simple(
        "web_fetch",
        "Web fetch",
        "Fetch an HTTP(S) URL and return its content as text (HTML pages are converted to plain text or markdown). Only public internet hosts are reachable.",
        schema_object(
            json!({
                "url": {"type": "string", "description": "Absolute http(s) URL to fetch"}
            }),
            &["url"],
        ),
        move |_id, args, cancel, _on_update| {
            let client = client.clone();
            let route = route.clone();
            async move {
                let raw_url = arg_str(&args, "url")?;
                match &route {
                    FetchRoute::Direct => fetch_url(&client, &raw_url, policy, cancel.as_ref()).await,
                    FetchRoute::Soket { endpoint, api_key } => {
                        fetch_via_soket(&client, endpoint, api_key, &raw_url, cancel.as_ref())
                            .await
                    }
                }
            }
        },
    ))
}

/// Fetch with validated redirects and a capped, truncated body.
async fn fetch_url(
    client: &reqwest::Client,
    raw_url: &str,
    policy: FetchPolicy,
    cancel: Option<&CancellationToken>,
) -> Result<AgentToolResult, String> {
    let mut url = Url::parse(raw_url).map_err(|e| format!("invalid URL '{raw_url}': {e}"))?;
    // Only remembered when a redirect actually happens (the common fetch does
    // not redirect, and `Url` clones are heap allocations).
    let mut redirected_from: Option<Url> = None;

    for _ in 0..=MAX_REDIRECTS {
        validate_request_url(&url, policy)?;
        let request = client.get(url.clone()).header(
            "accept",
            "text/html, application/json, application/xml, text/plain;q=0.9, */*;q=0.5",
        );
        let response = send_cancellable(request, cancel)
            .await
            .map_err(|e| format!("request to {} failed: {e}", url.as_str()))?;

        let status = response.status();
        if is_redirect(status) {
            let Some(location) = response
                .headers()
                .get(LOCATION)
                .and_then(|v| v.to_str().ok())
                .map(str::trim)
                .filter(|v| !v.is_empty())
            else {
                return Err(format!("redirect without Location from {}", url.as_str()));
            };
            let next = url
                .join(location)
                .map_err(|e| format!("invalid redirect location '{location}': {e}"))?;
            redirected_from.get_or_insert_with(|| url.clone());
            url = next;
            continue;
        }
        if !status.is_success() {
            return Err(format!("HTTP {} from {}", status.as_u16(), url.as_str()));
        }

        // An absent or empty header degrades to text/plain rather than a
        // confusing "unsupported content type ''".
        let header_content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            .to_ascii_lowercase();
        let content_type: &str = if header_content_type.is_empty() {
            "text/plain"
        } else {
            header_content_type.as_str()
        };
        let textual = content_type.starts_with("text/")
            || content_type.contains("json")
            || content_type.contains("xml")
            || content_type.contains("javascript");
        if !textual {
            return Err(format!(
                "unsupported content type '{content_type}' (expected text, HTML, JSON or XML)"
            ));
        }

        let (bytes, download_truncated) = read_capped(response, cancel).await?;
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        // Some servers prefix a UTF-8 byte-order mark; it is noise to a model.
        if let Some(without_bom) = text.strip_prefix('\u{feff}') {
            text = without_bom.to_string();
        }
        if content_type.contains("html") {
            text = html::html_to_text(&text);
        }
        let (text, truncated) = truncate_head(&text, DEFAULT_MAX_BYTES);
        let mut details = json!({
            "url": url.as_str(),
            "via": "direct",
            "status": status.as_u16(),
            "contentType": content_type,
            "bytes": bytes.len(),
            "truncated": truncated || download_truncated,
        });
        if let Some(original) = &redirected_from {
            details["redirectedFrom"] = json!(original.as_str());
        }
        return Ok(AgentToolResult {
            content: vec![ToolResultContent::Text(TextContent {
                text,
                text_signature: None,
            })],
            details,
            usage: None,
            added_tool_names: None,
            terminate: None,
        });
    }
    Err(format!("too many redirects (more than {MAX_REDIRECTS})"))
}

/// Fetch through Soket's fetch API (`POST {base}/fetch {"url"}`), which
/// returns page text in `content` (or `markdown` on older servers).
/// Failures are reported, not retried directly:
/// a silent fallback would fetch the page twice and from a different origin.
async fn fetch_via_soket(
    client: &reqwest::Client,
    endpoint: &Url,
    api_key: &str,
    raw_url: &str,
    cancel: Option<&CancellationToken>,
) -> Result<AgentToolResult, String> {
    let url = Url::parse(raw_url).map_err(|e| format!("invalid URL '{raw_url}': {e}"))?;
    // Soket cannot reach this machine's network, and private URLs should not
    // leave it: apply the strict policy whatever `allowPrivateAddresses` says.
    guard::validate_resolved_host(&url, FetchPolicy::default()).await?;
    let request = client
        .post(endpoint.clone())
        .bearer_auth(api_key)
        .json(&json!({ "url": url.as_str() }));
    let response = send_cancellable(request, cancel)
        .await
        .map_err(|e| format!("soket fetch of {} failed: {e}", url.as_str()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!(
            "soket fetch of {} failed: HTTP {status} (set webFetch.provider to \"direct\" to bypass Soket)",
            url.as_str()
        ));
    }
    let (bytes, too_large) = read_capped(response, cancel).await?;
    if too_large {
        return Err("soket fetch response is too large".into());
    }
    let body: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| format!("soket fetch returned invalid JSON: {e}"))?;
    search::check_soket_ok(&body, "fetch").map_err(|e| format!("{e} ({})", url.as_str()))?;
    let field = |key: &str| body.get(key).and_then(|v| v.as_str()).unwrap_or("");
    let content = ["content", "markdown"]
        .iter()
        .map(|key| field(key))
        .find(|text| !text.trim().is_empty())
        .ok_or_else(|| format!("soket fetch returned no page content for {}", url.as_str()))?;
    let title = field("title");
    let page = if title.trim().is_empty() {
        content.to_string()
    } else {
        format!("# {}\n\n{content}", title.trim())
    };
    let (text, truncated) = truncate_head(&page, DEFAULT_MAX_BYTES);
    // Only page fields are kept: the response also names Soket's egress proxy.
    let final_url = Some(field("url"))
        .filter(|u| !u.is_empty())
        .unwrap_or(url.as_str());
    let details = json!({
        "url": final_url,
        "via": "soket",
        "title": title,
        "contentType": "text/markdown",
        "bytes": content.len(),
        "truncated": truncated || body.get("truncated").and_then(serde_json::Value::as_bool).unwrap_or(false),
    });
    Ok(AgentToolResult {
        content: vec![ToolResultContent::Text(TextContent {
            text,
            text_signature: None,
        })],
        details,
        usage: None,
        added_tool_names: None,
        terminate: None,
    })
}

/// Send `request`, giving up as soon as `cancel` fires.
pub(crate) async fn send_cancellable(
    request: reqwest::RequestBuilder,
    cancel: Option<&CancellationToken>,
) -> Result<reqwest::Response, String> {
    let send = request.send();
    match cancel {
        Some(token) => tokio::select! {
            biased;
            () = token.cancelled() => Err("cancelled".into()),
            result = send => result.map_err(|e| e.to_string()),
        },
        None => send.await.map_err(|e| e.to_string()),
    }
}

fn is_redirect(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308)
}

/// Stream the body up to [`MAX_DOWNLOAD_BYTES`], honouring cancellation.
pub(crate) async fn read_capped(
    mut response: reqwest::Response,
    cancel: Option<&CancellationToken>,
) -> Result<(Vec<u8>, bool), String> {
    // Pre-size from Content-Length (capped) to avoid growth reallocations.
    let expected = response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(8 * 1024);
    let mut body = Vec::with_capacity(expected.min(MAX_DOWNLOAD_BYTES));
    loop {
        let next_chunk = match cancel {
            Some(token) => tokio::select! {
                biased;
                () = token.cancelled() => return Err("web_fetch cancelled".into()),
                chunk = response.chunk() => chunk,
            },
            None => response.chunk().await,
        }
        .map_err(|e| format!("download interrupted: {e}"))?;
        let Some(chunk) = next_chunk else {
            break;
        };
        let remaining = MAX_DOWNLOAD_BYTES.saturating_sub(body.len());
        if chunk.len() >= remaining {
            body.extend_from_slice(&chunk[..remaining]);
            return Ok((body, true));
        }
        body.extend_from_slice(&chunk);
    }
    Ok((body, false))
}

/// Format a search result list the way `web_search` returns it.
pub(crate) fn format_hits(hits: &[search::SearchHit]) -> String {
    hits.iter()
        .enumerate()
        .map(|(i, hit)| {
            format!(
                "{}. {}\n   {}\n   {}",
                i + 1,
                hit.title,
                hit.url,
                hit.snippet
            )
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub(crate) use crate::harness::tools::{arg_str, schema_object};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_config_maps_to_policy() {
        assert_eq!(
            WebFetchConfig::default().policy(),
            FetchPolicy {
                allow_private_addresses: false
            }
        );
        assert_eq!(
            WebFetchConfig {
                allow_private_addresses: true,
                ..Default::default()
            }
            .policy(),
            FetchPolicy {
                allow_private_addresses: true
            }
        );
    }

    #[test]
    fn hit_formatting() {
        let hits = vec![search::SearchHit {
            title: "Rust".into(),
            url: "https://rust-lang.org".into(),
            snippet: "A language".into(),
        }];
        assert_eq!(
            format_hits(&hits),
            "1. Rust\n   https://rust-lang.org\n   A language"
        );
    }
}
