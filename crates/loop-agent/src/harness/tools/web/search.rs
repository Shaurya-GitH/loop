//! Web search backends for [`super::web_search`].
//!
//! A backend is a small REST call plus a response parser, selected by
//! `webSearch.provider` in settings. Backends are resolved eagerly at tool
//! creation so a missing API key is reported once, not on every query.

use loop_ai::providers::{SOKET_API_KEY_ENVS, SOKET_BASE_URL, SOKET_PROVIDER_ID};
use reqwest::StatusCode;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fmt;
use tokio_util::sync::CancellationToken;

use super::guard::{validate_request_url, FetchPolicy};

/// Backend used when `webSearch.provider` is unset: Soket, with the key the
/// user already stored for the Soket model provider.
pub const DEFAULT_WEB_SEARCH_PROVIDER: &str = SOKET_PROVIDER_ID;

/// Configuration for the `web_search` tool (`webSearch` in settings.json).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct WebSearchConfig {
    /// Whether the web tools (`web_search` and `web_fetch`) are registered.
    /// Defaults to on; `/websearch off` and `--no-web` turn them off.
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Backend id (`soket`, `tavily`, `tinyfish`, `brave`); unset means
    /// [`DEFAULT_WEB_SEARCH_PROVIDER`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Env vars that may hold the API key, checked in order. Falls back to the
    /// backend's default env vars (e.g. `SOKET_API_KEY`, `TAVILY_API_KEY`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub api_key_env: Vec<String>,
    /// Override the backend's API base URL (self-hosted proxies).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Allow loopback/private base URLs. Intended for self-hosted mirrors; the
    /// default fails closed.
    #[serde(default)]
    pub allow_private_addresses: bool,
}

fn default_enabled() -> bool {
    true
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: None,
            api_key_env: Vec::new(),
            base_url: None,
            allow_private_addresses: false,
        }
    }
}

impl WebSearchConfig {
    /// The selected backend id, lowercased (the default when unset).
    pub fn provider_id(&self) -> String {
        self.provider
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .unwrap_or(DEFAULT_WEB_SEARCH_PROVIDER)
            .to_ascii_lowercase()
    }
}

/// One search hit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchHit {
    /// Result title.
    pub title: String,
    /// Result URL.
    pub url: String,
    /// Short snippet from the page.
    pub snippet: String,
}

/// Built-in backend registry — the single source of truth for backend ids,
/// display names and defaults. `/login` validation (loop-app-core) and the
/// login wizard (loop-cli) consult it; adding a backend is one entry here.
#[derive(Debug)]
pub struct WebSearchBackendInfo {
    /// Backend id (`webSearch.provider`).
    pub id: &'static str,
    /// Human-readable name.
    pub display_name: &'static str,
    /// Env vars checked when neither `apiKeyEnv` nor a stored key resolves.
    pub default_api_key_envs: &'static [&'static str],
    /// API base URL unless `webSearch.baseUrl` overrides it.
    pub default_base_url: &'static str,
    /// The key is the one stored for the model provider of the same id, so
    /// `/login <id>` and `/logout <id>` belong to the model-provider flow and
    /// the backend gets no row of its own in the picker.
    pub shares_model_provider_key: bool,
}

impl WebSearchBackendInfo {
    /// The env var named in "set …" hints.
    pub fn primary_api_key_env(&self) -> &'static str {
        self.default_api_key_envs.first().copied().unwrap_or("")
    }
}

/// The built-in web-search backends, default first.
pub const WEB_SEARCH_BACKENDS: &[WebSearchBackendInfo] = &[
    WebSearchBackendInfo {
        id: SOKET_PROVIDER_ID,
        display_name: "Soket",
        default_api_key_envs: SOKET_API_KEY_ENVS,
        default_base_url: SOKET_BASE_URL,
        shares_model_provider_key: true,
    },
    WebSearchBackendInfo {
        id: "tavily",
        display_name: "Tavily",
        default_api_key_envs: &["TAVILY_API_KEY"],
        default_base_url: "https://api.tavily.com",
        shares_model_provider_key: false,
    },
    WebSearchBackendInfo {
        id: "tinyfish",
        display_name: "TinyFish",
        default_api_key_envs: &["TINYFISH_API_KEY"],
        default_base_url: "https://api.search.tinyfish.ai",
        shares_model_provider_key: false,
    },
    WebSearchBackendInfo {
        id: "brave",
        display_name: "Brave Search",
        default_api_key_envs: &["BRAVE_API_KEY"],
        default_base_url: "https://api.search.brave.com",
        shares_model_provider_key: false,
    },
];

/// Look up a built-in backend by id (case-insensitive, whitespace trimmed).
pub fn web_search_backend(id: &str) -> Option<&'static WebSearchBackendInfo> {
    WEB_SEARCH_BACKENDS
        .iter()
        .find(|info| info.id.eq_ignore_ascii_case(id.trim()))
}

fn backend_ids() -> String {
    WEB_SEARCH_BACKENDS
        .iter()
        .map(|info| format!("'{}'", info.id))
        .collect::<Vec<_>>()
        .join(", ")
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BackendKind {
    Soket,
    Tavily,
    TinyFish,
    Brave,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SearchFailureKind {
    Authentication,
    RateLimited,
    Upstream,
    Other,
}

#[derive(Debug)]
struct SearchFailure {
    message: String,
    kind: SearchFailureKind,
}

impl SearchFailure {
    fn message(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: SearchFailureKind::Other,
        }
    }

    fn http(backend: &str, status: StatusCode) -> Self {
        let kind = match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => SearchFailureKind::Authentication,
            StatusCode::TOO_MANY_REQUESTS => SearchFailureKind::RateLimited,
            status if status.is_server_error() => SearchFailureKind::Upstream,
            _ => SearchFailureKind::Other,
        };
        Self {
            message: format!("{backend} search failed: HTTP {status}"),
            kind,
        }
    }
}

impl fmt::Display for SearchFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl From<String> for SearchFailure {
    fn from(message: String) -> Self {
        Self::message(message)
    }
}

#[derive(Clone)]
struct SearchBackend {
    kind: BackendKind,
    id: &'static str,
    base_url: Url,
    api_key: String,
}

impl SearchBackend {
    /// Resolve the backend from config. The API key comes from `apiKeyEnv`
    /// env vars, the stored key (auth.json), or the backend's default env vars,
    /// in that order. `Err` explains why the tool cannot be created.
    fn resolve(config: &WebSearchConfig, stored_key: Option<&str>) -> Result<Self, String> {
        let provider = config.provider_id();
        let info = web_search_backend(&provider).ok_or_else(|| {
            format!(
                "unknown provider '{provider}' (expected one of {})",
                backend_ids()
            )
        })?;
        let api_key =
            resolve_api_key(config, stored_key, info.default_api_key_envs).ok_or_else(|| {
                format!(
                    "no API key: run /login {}, or set {}",
                    info.id,
                    info.primary_api_key_env()
                )
            })?;
        Self::new(info, api_key, config)
    }

    /// Build a backend for an explicit registry record and key (the
    /// verification path also uses this).
    fn new(
        info: &'static WebSearchBackendInfo,
        api_key: String,
        config: &WebSearchConfig,
    ) -> Result<Self, String> {
        if api_key.trim().is_empty() {
            return Err(format!("{}: API key is empty", info.id));
        }
        let kind = match info.id {
            SOKET_PROVIDER_ID => BackendKind::Soket,
            "tavily" => BackendKind::Tavily,
            "tinyfish" => BackendKind::TinyFish,
            "brave" => BackendKind::Brave,
            other => return Err(format!("backend '{other}' has no request implementation")),
        };
        let policy = FetchPolicy {
            allow_private_addresses: config.allow_private_addresses,
        };
        let base_url = resolve_base_url(
            config.base_url.as_deref().unwrap_or(info.default_base_url),
            policy,
        )?;
        Ok(Self {
            kind,
            id: info.id,
            base_url,
            api_key,
        })
    }

    /// Run a search; the hit list is cut to `max_results` (Soket and TinyFish
    /// may return more than asked for).
    async fn search(
        &self,
        client: &reqwest::Client,
        query: &str,
        max_results: usize,
        cancel: Option<&CancellationToken>,
    ) -> Result<Vec<SearchHit>, SearchFailure> {
        let id = self.id;
        let request = match self.kind {
            BackendKind::Soket | BackendKind::Tavily => client
                .post(endpoint(&self.base_url, "search"))
                .bearer_auth(&self.api_key)
                .json(&json!({ "query": query, "max_results": max_results })),
            BackendKind::TinyFish => {
                // No result-count parameter; the list is cut below.
                let mut url = self.base_url.clone();
                url.query_pairs_mut().append_pair("query", query);
                client.get(url).header("X-API-Key", &self.api_key)
            }
            BackendKind::Brave => {
                let mut url = endpoint(&self.base_url, "res/v1/web/search");
                url.query_pairs_mut()
                    .append_pair("q", query)
                    .append_pair("count", &max_results.to_string());
                client
                    .get(url)
                    .header("X-Subscription-Token", &self.api_key)
            }
        };
        let response = super::send_cancellable(request, cancel)
            .await
            .map_err(|e| SearchFailure::message(format!("{id} search failed: {e}")))?;
        let body = read_json(response, id, cancel).await?;
        let mut hits = match self.kind {
            BackendKind::Soket => parse_soket(&body)?,
            BackendKind::Tavily => parse_tavily(&body)?,
            BackendKind::TinyFish => parse_tinyfish(&body)?,
            BackendKind::Brave => parse_brave(&body)?,
        };
        hits.truncate(max_results);
        Ok(hits)
    }
}

/// `base` with `path` appended to its own path, so a `baseUrl` override with a
/// prefix (`https://proxy.internal/tavily`) keeps it.
pub(super) fn endpoint(base: &Url, path: &str) -> Url {
    let mut url = base.clone();
    let joined = format!(
        "{}/{}",
        base.path().trim_end_matches('/'),
        path.trim_start_matches('/')
    );
    url.set_path(&joined);
    url
}

/// Check the status first (error bodies are often not JSON), then read a
/// capped body and parse it.
async fn read_json(
    response: reqwest::Response,
    backend: &str,
    cancel: Option<&CancellationToken>,
) -> Result<Value, SearchFailure> {
    let status = response.status();
    if !status.is_success() {
        return Err(SearchFailure::http(backend, status));
    }
    let (bytes, truncated) = super::read_capped(response, cancel).await?;
    if truncated {
        return Err(SearchFailure::message(format!(
            "{backend} response is too large"
        )));
    }
    serde_json::from_slice(&bytes)
        .map_err(|e| SearchFailure::message(format!("{backend} returned invalid JSON: {e}")))
}

/// API key resolution order: `apiKeyEnv` env vars → stored key → default envs.
fn resolve_api_key(
    config: &WebSearchConfig,
    stored_key: Option<&str>,
    default_envs: &[&str],
) -> Option<String> {
    let non_empty = |v: String| (!v.trim().is_empty()).then_some(v);
    config
        .api_key_env
        .iter()
        .find_map(|name| std::env::var(name).ok().and_then(non_empty))
        .or_else(|| stored_key.and_then(|k| non_empty(k.to_string())))
        .or_else(|| {
            default_envs
                .iter()
                .find_map(|name| std::env::var(name).ok().and_then(non_empty))
        })
}

fn parse_base_url(raw: &str) -> Result<Url, String> {
    let url = Url::parse(raw).map_err(|_| format!("invalid base URL '{raw}'"))?;
    if url.host_str().is_none() {
        return Err(format!("invalid base URL '{raw}': no host"));
    }
    Ok(url)
}

/// Parse a configured base URL and apply the same SSRF rules as fetches, so a
/// `baseUrl` override cannot point the search tool at internal addresses
/// unless explicitly allowed.
pub(super) fn resolve_base_url(raw: &str, policy: FetchPolicy) -> Result<Url, String> {
    let url = parse_base_url(raw)?;
    validate_request_url(&url, policy).map_err(|e| format!("invalid base URL '{raw}': {e}"))?;
    Ok(url)
}

/// Hits from `body[list]`, trying `snippet_fields` in order for the snippet. Entries
/// without a title or URL are dropped.
fn parse_hits(
    body: &Value,
    pointer: &str,
    snippet_fields: &[&str],
    backend: &str,
) -> Result<Vec<SearchHit>, String> {
    let results = body
        .pointer(pointer)
        .and_then(Value::as_array)
        .ok_or_else(|| {
            let field = pointer.trim_start_matches('/').replace('/', ".");
            format!("{backend} response is missing '{field}'")
        })?;
    Ok(results
        .iter()
        .filter_map(|item| {
            Some(SearchHit {
                title: item.get("title")?.as_str()?.to_string(),
                url: item.get("url")?.as_str()?.to_string(),
                snippet: snippet_fields
                    .iter()
                    .find_map(|field| {
                        item.get(*field)
                            .and_then(Value::as_str)
                            .filter(|text| !text.trim().is_empty())
                    })
                    .unwrap_or_default()
                    .to_string(),
            })
        })
        .collect())
}

/// Soket reports failures in the body as `{"ok": false, "message": …}`.
pub(super) fn check_soket_ok(body: &Value, what: &str) -> Result<(), String> {
    if body.get("ok").and_then(Value::as_bool) == Some(false) {
        let detail = ["message", "error_kind"]
            .iter()
            .find_map(|key| body.get(*key).and_then(Value::as_str))
            .unwrap_or("no details");
        return Err(format!("soket {what} failed: {detail}"));
    }
    Ok(())
}

fn parse_soket(body: &Value) -> Result<Vec<SearchHit>, String> {
    check_soket_ok(body, "search")?;
    parse_hits(body, "/results", &["snippet", "description"], "soket")
}

fn parse_tavily(body: &Value) -> Result<Vec<SearchHit>, String> {
    parse_hits(body, "/results", &["content"], "tavily")
}

fn parse_tinyfish(body: &Value) -> Result<Vec<SearchHit>, String> {
    parse_hits(body, "/results", &["snippet"], "tinyfish")
}

fn parse_brave(body: &Value) -> Result<Vec<SearchHit>, String> {
    parse_hits(body, "/web/results", &["description"], "brave")
}

/// Build the `web_search` tool. `stored_key` is the key saved by `/login`
/// (auth.json; for Soket, the model provider's key); env vars from
/// `apiKeyEnv` take precedence over it. `Err` (missing key, unknown provider,
/// bad base URL) should be reported by the caller and the tool left
/// unregistered.
pub fn create_web_search_tool(
    config: WebSearchConfig,
    stored_key: Option<&str>,
) -> Result<crate::types::AgentTool, String> {
    let backend = SearchBackend::resolve(&config, stored_key)?;
    let policy = FetchPolicy {
        allow_private_addresses: config.allow_private_addresses,
    };
    let client = super::shared_client(policy)?;
    Ok(crate::types::AgentTool::simple(
        "web_search",
        "Web search",
        "Search the web and return ranked results (title, URL, snippet). Use web_fetch to read a result's page.",
        super::schema_object(
            json!({
                "query": {"type": "string", "description": "Search query"},
                "max_results": {"type": "integer", "description": "Results to return (1-10)", "minimum": 1, "maximum": 10}
            }),
            &["query"],
        ),
        move |_id, args, cancel, _on_update| {
            let backend = backend.clone();
            let client = client.clone();
            async move {
                let query = super::arg_str(&args, "query")?;
                let max_results = args
                    .get("max_results")
                    .and_then(Value::as_u64)
                    .unwrap_or(5)
                    .clamp(1, 10) as usize;
                let hits = backend
                    .search(&client, &query, max_results, cancel.as_ref())
                    .await
                    .map_err(|error| error.to_string())?;
                if hits.is_empty() {
                    return Ok(crate::types::AgentToolResult::text(format!(
                        "No results for '{query}'."
                    )));
                }
                let text = super::format_hits(&hits);
                let details = json!({
                    "provider": backend.id,
                    "query": query,
                    "results": hits.iter().map(|hit| json!({
                        "title": hit.title, "url": hit.url, "snippet": hit.snippet
                    })).collect::<Vec<_>>()
                });
                Ok(crate::types::AgentToolResult {
                    content: vec![loop_ai::ToolResultContent::Text(loop_ai::TextContent {
                        text,
                        text_signature: None,
                    })],
                    details,
                    usage: None,
                    added_tool_names: None,
                    terminate: None,
                })
            }
        },
    ))
}

/// Verify an API key before saving it: run a one-result probe search.
/// Auth failures (401/403) are reported as a rejected key; a probe costs one
/// query on the backend's free tier.
pub async fn verify_web_search_key(config: &WebSearchConfig, api_key: &str) -> Result<(), String> {
    let provider = config.provider_id();
    let info = web_search_backend(&provider).ok_or_else(|| {
        format!(
            "unknown provider '{provider}' (expected one of {})",
            backend_ids()
        )
    })?;
    let backend = SearchBackend::new(info, api_key.to_string(), config)?;
    let policy = FetchPolicy {
        allow_private_addresses: config.allow_private_addresses,
    };
    let client = super::shared_client(policy)?;
    match backend
        .search(&client, "loop agent connectivity check", 1, None)
        .await
    {
        Ok(_) => Ok(()),
        Err(error) if error.kind == SearchFailureKind::Authentication => {
            Err(format!("{} rejected the API key", info.display_name))
        }
        Err(error) => Err(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tavily_results() {
        let body: Value = serde_json::from_str(
            r#"{"query":"rust","results":[
                {"title":"Rust","url":"https://rust-lang.org","content":"A language"},
                {"title":"Docs","url":"https://doc.rust-lang.org","content":"Learn"}
            ], "answer":"ok"}"#,
        )
        .unwrap();
        let hits = parse_tavily(&body).unwrap();
        assert_eq!(hits.len(), 2);
        assert_eq!(
            hits[0],
            SearchHit {
                title: "Rust".into(),
                url: "https://rust-lang.org".into(),
                snippet: "A language".into(),
            }
        );
    }

    #[test]
    fn parses_brave_results() {
        let body: Value = serde_json::from_str(
            r#"{"web":{"results":[
                {"title":"Rust","url":"https://rust-lang.org","description":"Systems lang",
                 "meta":{"foo":1}}
            ]}}"#,
        )
        .unwrap();
        let hits = parse_brave(&body).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].snippet, "Systems lang");
    }

    #[test]
    fn missing_results_is_an_error() {
        let body: Value = serde_json::from_str(r#"{"web":{}}"#).unwrap();
        assert!(parse_brave(&body).is_err());
        let body: Value = serde_json::from_str(r#"{}"#).unwrap();
        assert!(parse_tavily(&body).is_err());
    }

    #[test]
    fn entries_missing_required_fields_are_dropped() {
        let body: Value = serde_json::from_str(
            r#"{"results":[
                {"title":"ok","url":"https://ok","content":"c"},
                {"title":"no url"},
                {"url":"https://no-title","content":"c"},
                {"title":"null url","url":null}
            ]}"#,
        )
        .unwrap();
        let hits = parse_tavily(&body).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].url, "https://ok");
    }

    #[test]
    fn whitespace_only_keys_are_treated_as_missing() {
        std::env::remove_var("LOOP_TEST_UNSET_SEARCH_KEY");
        let config = WebSearchConfig {
            provider: Some("tavily".into()),
            ..Default::default()
        };
        let unset = &["LOOP_TEST_UNSET_SEARCH_KEY"];
        assert!(
            resolve_api_key(&config, Some("   "), unset).is_none(),
            "a whitespace-only stored key must not count"
        );
        assert!(resolve_api_key(&config, Some("real-key"), unset).is_some());
    }

    #[test]
    fn endpoints_keep_the_base_path() {
        let url = |s: &str| Url::parse(s).unwrap();
        assert_eq!(
            endpoint(&url("https://api.soket.ai/v1"), "search").as_str(),
            "https://api.soket.ai/v1/search"
        );
        assert_eq!(
            endpoint(&url("https://proxy.example/tavily/"), "/search").as_str(),
            "https://proxy.example/tavily/search"
        );
        assert_eq!(
            endpoint(&url("https://api.tavily.com"), "search").as_str(),
            "https://api.tavily.com/search"
        );
    }

    #[test]
    fn registry_ids_all_have_a_request_implementation() {
        let config = WebSearchConfig::default();
        for info in WEB_SEARCH_BACKENDS {
            assert!(
                SearchBackend::new(info, "k".into(), &config).is_ok(),
                "{} has no request implementation",
                info.id
            );
        }
        assert_eq!(WEB_SEARCH_BACKENDS[0].id, DEFAULT_WEB_SEARCH_PROVIDER);
    }

    #[test]
    fn soket_failures_carry_the_message() {
        let body = json!({"ok": false, "message": "quota exceeded", "results": []});
        let err = parse_soket(&body).unwrap_err();
        assert!(err.contains("quota exceeded"), "{err}");
        let body = json!({"ok": false, "error_kind": "blocked"});
        assert!(check_soket_ok(&body, "fetch")
            .unwrap_err()
            .contains("blocked"));
        // Responses without `ok` are judged by their payload alone.
        let body = json!({"results": [{"title": "t", "url": "https://t", "description": "d"}]});
        assert_eq!(parse_soket(&body).unwrap()[0].snippet, "d");
    }

    #[test]
    fn base_urls_must_have_a_host() {
        assert!(parse_base_url("https://api.tavily.com").is_ok());
        assert!(parse_base_url("not a url").is_err());
    }
}
