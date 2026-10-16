//! Soket and TinyFish web backends against the loop-test-support fake server.
//!
//! Keys are passed in directly (the stored-key parameter or `SoketWebApi`),
//! never read from the default env vars, so an exported `SOKET_API_KEY` on a
//! developer machine cannot change the results. The fake server listens on
//! 127.0.0.1, so its base URLs need `allowPrivateAddresses`.

use loop_agent::harness::{
    create_web_fetch_tool, create_web_search_tool, verify_web_search_key, SoketWebApi,
    WebFetchConfig, WebFetchProvider, WebSearchConfig,
};
use loop_agent::types::{AgentTool, AgentToolResult};
use loop_test_support::{FakeHttpServer, FakeResponse};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

/// Legacy search response shape (trimmed). Current fields are tested below.
const SOKET_SEARCH: &str = r#"{
    "query": "soket ai news", "engine": "duckduckgo", "proxy": "http://user:***@192.0.2.1:1",
    "count": 3, "elapsed_ms": 1831, "cached": false,
    "results": [
        {"rank": 1, "url": "https://soket.ai/", "title": "Soket AI", "description": "Frontier AI", "engine": "duckduckgo"},
        {"rank": 2, "url": "https://soket.ai/blogs", "title": "Blog", "description": "Posts", "engine": "duckduckgo"},
        {"rank": 3, "url": "https://example.org", "title": "Other", "description": "More", "engine": "duckduckgo"}
    ],
    "attempts": null, "message": null, "ok": true
}"#;

/// Legacy fetch response shape. Current `content` responses are tested below.
const SOKET_FETCH: &str = r#"{
    "url": "https://example.com", "title": "Example Domain",
    "markdown": "This domain is for use in documentation examples.",
    "elapsed_ms": 642, "proxy": "http://user:***@192.0.2.1:1", "llm_cleaned": false,
    "content_length": 50, "llm_available": false, "message": null, "error_kind": null, "ok": true
}"#;

/// Assumed failure shape (`ok: false` plus `message` / `error_kind`); not
/// observed live.
const SOKET_NOT_OK: &str =
    r#"{"ok": false, "message": "upstream timed out", "error_kind": "timeout", "results": []}"#;

async fn execute(tool: &AgentTool, args: Value) -> Result<AgentToolResult, String> {
    (tool.execute)("test".into(), args, None, None).await
}

fn text(result: &AgentToolResult) -> String {
    match result.content.first() {
        Some(loop_ai::ToolResultContent::Text(text)) => text.text.clone(),
        _ => String::new(),
    }
}

fn creation_error(result: Result<AgentTool, String>) -> String {
    match result {
        Ok(_) => panic!("expected tool creation to fail"),
        Err(error) => error,
    }
}

/// A search config for `provider` (`None` = the Soket default) aimed at the
/// fake server under `/v1`, like the real Soket base URL.
fn search_config(server: &FakeHttpServer, provider: Option<&str>) -> WebSearchConfig {
    WebSearchConfig {
        provider: provider.map(String::from),
        base_url: Some(format!("{}/v1", server.base_url())),
        allow_private_addresses: true,
        ..Default::default()
    }
}

fn soket_api(server: &FakeHttpServer) -> SoketWebApi {
    SoketWebApi {
        base_url: format!("{}/v1", server.base_url()),
        api_key: "sk-test".into(),
    }
}

/// Soket fetch config that may reach the fake server's base URL.
fn soket_fetch_config() -> WebFetchConfig {
    WebFetchConfig {
        allow_private_addresses: true,
        provider: WebFetchProvider::Soket,
    }
}

#[tokio::test]
async fn soket_is_the_default_search_backend() {
    let server = FakeHttpServer::always(FakeResponse::json(SOKET_SEARCH));
    let tool = create_web_search_tool(search_config(&server, None), Some("sk-test"))
        .expect("web_search tool");
    let result = execute(&tool, json!({"query": "soket ai news", "max_results": 2}))
        .await
        .expect("search ok");

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/v1/search", "base path prefix is kept");
    assert_eq!(requests[0].header("authorization"), Some("Bearer sk-test"));
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body, json!({"query": "soket ai news", "max_results": 2}));

    // Soket may return more hits than asked for; the list is cut.
    let text = text(&result);
    assert!(text.contains("1. Soket AI\n   https://soket.ai/\n   Frontier AI"));
    assert!(text.contains("2. Blog"));
    assert!(!text.contains("Other"));
    assert_eq!(result.details["provider"], "soket");
    assert_eq!(result.details["results"].as_array().unwrap().len(), 2);
    assert!(!result.details.to_string().contains("proxy"));
}

#[tokio::test]
async fn soket_search_reports_body_level_failures() {
    let server = FakeHttpServer::always(FakeResponse::json(SOKET_NOT_OK));
    let tool = create_web_search_tool(search_config(&server, Some("soket")), Some("sk-test"))
        .expect("web_search tool");
    let err = execute(&tool, json!({"query": "q"}))
        .await
        .expect_err("ok:false must fail");
    assert!(
        err.contains("upstream timed out"),
        "unexpected error: {err}"
    );
}

#[tokio::test]
async fn non_json_auth_errors_are_reported_as_rejected_keys() {
    let server = FakeHttpServer::always(FakeResponse::new(
        401,
        "text/html",
        "<html><body>401 Authorization Required</body></html>",
    ));
    let err = verify_web_search_key(&search_config(&server, Some("tavily")), "tvly-bad")
        .await
        .expect_err("401 must fail verification");
    assert!(
        err.contains("rejected the API key"),
        "unexpected error: {err}"
    );

    let tool = create_web_search_tool(search_config(&server, None), Some("sk-bad"))
        .expect("web_search tool");
    let err = execute(&tool, json!({"query": "q"}))
        .await
        .expect_err("401 must fail the search");
    assert!(err.contains("HTTP 401"), "unexpected error: {err}");
}

#[tokio::test]
async fn rate_limit_and_server_errors_are_not_classified_as_bad_keys() {
    for status in [429, 503] {
        let server = FakeHttpServer::always(FakeResponse::new(
            status,
            "text/plain",
            "upstream unavailable",
        ));
        let err = verify_web_search_key(&search_config(&server, Some("tavily")), "tvly-key")
            .await
            .expect_err("non-authentication HTTP errors must fail verification");
        assert!(
            !err.contains("rejected the API key"),
            "unexpected error: {err}"
        );
        assert!(
            err.contains(&format!("HTTP {status}")),
            "unexpected error: {err}"
        );
    }
}

#[tokio::test]
async fn tinyfish_search_backend() {
    let server = FakeHttpServer::always(FakeResponse::json(
        r#"{"query":"rust","results":[
            {"position":1,"site_name":"rust-lang.org","title":"Rust","snippet":"A language","url":"https://rust-lang.org"},
            {"position":2,"site_name":"docs.rs","title":"Docs","snippet":"Crates","url":"https://docs.rs"}
        ],"total_results":2,"page":0}"#,
    ));
    let mut config = search_config(&server, Some("tinyfish"));
    config.base_url = Some(server.base_url().to_string());
    let tool = create_web_search_tool(config, Some("tf-test")).expect("web_search tool");
    let result = execute(&tool, json!({"query": "rust lang", "max_results": 1}))
        .await
        .expect("search ok");

    let requests = server.requests();
    assert_eq!(requests[0].method, "GET");
    assert_eq!(requests[0].path, "/?query=rust+lang");
    assert_eq!(requests[0].header("x-api-key"), Some("tf-test"));
    let text = text(&result);
    assert!(text.contains("1. Rust\n   https://rust-lang.org\n   A language"));
    assert!(
        !text.contains("Docs"),
        "no count parameter, so the list is cut"
    );
    assert_eq!(result.details["provider"], "tinyfish");
}

#[tokio::test]
async fn soket_fetch_returns_markdown() {
    let server = FakeHttpServer::always(FakeResponse::json(SOKET_FETCH));
    let tool = create_web_fetch_tool(soket_fetch_config(), Some(soket_api(&server)))
        .expect("web_fetch tool");
    let result = execute(&tool, json!({"url": "https://example.com"}))
        .await
        .expect("fetch ok");

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/v1/fetch");
    assert_eq!(requests[0].header("authorization"), Some("Bearer sk-test"));
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body, json!({"url": "https://example.com/"}));

    assert_eq!(
        text(&result),
        "# Example Domain\n\nThis domain is for use in documentation examples."
    );
    assert_eq!(result.details["via"], "soket");
    assert_eq!(result.details["url"], "https://example.com");
    assert!(!result.details.to_string().contains("proxy"));
}

#[tokio::test]
async fn soket_fetch_reports_failures() {
    let not_ok = FakeHttpServer::always(FakeResponse::json(SOKET_NOT_OK));
    let tool = create_web_fetch_tool(soket_fetch_config(), Some(soket_api(&not_ok)))
        .expect("web_fetch tool");
    let err = execute(&tool, json!({"url": "https://example.com/slow"}))
        .await
        .expect_err("ok:false must fail");
    assert!(
        err.contains("upstream timed out"),
        "unexpected error: {err}"
    );
    assert!(
        err.contains("https://example.com/slow"),
        "unexpected error: {err}"
    );

    let down = FakeHttpServer::always(FakeResponse::new(502, "text/html", "<h1>Bad gateway</h1>"));
    let tool = create_web_fetch_tool(soket_fetch_config(), Some(soket_api(&down)))
        .expect("web_fetch tool");
    let err = execute(&tool, json!({"url": "https://example.com"}))
        .await
        .expect_err("502 must fail");
    assert!(err.contains("HTTP 502"), "unexpected error: {err}");
    assert!(err.contains("\"direct\""), "names the escape hatch: {err}");
    // No silent direct retry: only the one Soket request was made.
    assert_eq!(down.requests().len(), 1);
}

#[tokio::test]
async fn soket_fetch_never_forwards_private_targets() {
    let server = FakeHttpServer::always(FakeResponse::json(SOKET_FETCH));
    // `allowPrivateAddresses` admits the fake base URL but not the targets.
    let tool = create_web_fetch_tool(soket_fetch_config(), Some(soket_api(&server)))
        .expect("web_fetch tool");
    for target in [
        "http://127.0.0.1/admin",
        "http://localhost:8080/",
        "http://169.254.169.254/latest/meta-data",
        "http://[::ffff:10.0.0.1]/",
        "file:///etc/passwd",
    ] {
        assert!(
            execute(&tool, json!({"url": target})).await.is_err(),
            "{target} must be refused"
        );
    }
    assert!(server.requests().is_empty(), "nothing reached Soket");
}

#[tokio::test]
async fn soket_fetch_rejects_unresolvable_hostnames_before_forwarding() {
    let server = FakeHttpServer::always(FakeResponse::json(SOKET_FETCH));
    let tool = create_web_fetch_tool(soket_fetch_config(), Some(soket_api(&server)))
        .expect("web_fetch tool");
    let err = execute(&tool, json!({"url": "https://loop-internal-test.invalid/"}))
        .await
        .expect_err("unresolvable hostname must be rejected");
    assert!(err.contains("resolve"), "unexpected error: {err}");
    assert!(
        server.requests().is_empty(),
        "Soket must not receive the URL"
    );
}

#[tokio::test]
async fn fetch_path_selection() {
    let page = FakeHttpServer::always(FakeResponse::new(200, "text/plain", "direct page"));
    let page_url = format!("{}/page", page.base_url());
    let soket = FakeHttpServer::always(FakeResponse::json(SOKET_FETCH));

    // Auto without a Soket key: direct.
    let auto = WebFetchConfig {
        allow_private_addresses: true,
        provider: WebFetchProvider::Auto,
    };
    let tool = create_web_fetch_tool(auto, None).expect("web_fetch tool");
    let result = execute(&tool, json!({"url": page_url})).await.unwrap();
    assert_eq!(result.details["via"], "direct");
    assert_eq!(text(&result), "direct page");

    // Auto with a key but private addresses allowed (intranet): still direct.
    let tool = create_web_fetch_tool(auto, Some(soket_api(&soket))).expect("web_fetch tool");
    let result = execute(&tool, json!({"url": page_url})).await.unwrap();
    assert_eq!(result.details["via"], "direct");

    // Direct pinned: the key is ignored.
    let direct = WebFetchConfig {
        provider: WebFetchProvider::Direct,
        ..auto
    };
    let tool = create_web_fetch_tool(direct, Some(soket_api(&soket))).expect("web_fetch tool");
    let result = execute(&tool, json!({"url": page_url})).await.unwrap();
    assert_eq!(result.details["via"], "direct");
    assert!(soket.requests().is_empty());

    // Soket pinned without a key: the tool is not built.
    let err = creation_error(create_web_fetch_tool(soket_fetch_config(), None));
    assert!(err.contains("/login soket"), "unexpected error: {err}");
}

#[test]
fn soket_key_prefers_the_stored_credential() {
    let api = SoketWebApi::resolve(Some("sk-stored")).expect("stored key");
    assert_eq!(api.api_key, "sk-stored");
    assert_eq!(api.base_url, loop_ai::providers::SOKET_BASE_URL);
    assert!(
        !format!("{api:?}").contains("sk-stored"),
        "Debug hides the key"
    );
}

#[tokio::test]
async fn cancelled_requests_stop_before_sending() {
    let server = FakeHttpServer::always(FakeResponse::json(SOKET_SEARCH));
    let tool = create_web_search_tool(search_config(&server, None), Some("sk-test"))
        .expect("web_search tool");
    let cancel = CancellationToken::new();
    cancel.cancel();
    let err = (tool.execute)("test".into(), json!({"query": "q"}), Some(cancel), None)
        .await
        .expect_err("cancelled search must fail");
    assert!(err.contains("cancelled"), "unexpected error: {err}");
}

#[test]
fn old_settings_still_parse() {
    // Settings written by the first version of `webSearch` (provider only).
    let config: WebSearchConfig = serde_json::from_str(r#"{"provider":"tavily"}"#).unwrap();
    assert!(config.enabled);
    assert_eq!(config.provider_id(), "tavily");
    // `{}` and `{"enabled": false}` select the Soket default.
    let config: WebSearchConfig = serde_json::from_str(r#"{"enabled":false}"#).unwrap();
    assert!(!config.enabled);
    assert_eq!(config.provider_id(), "soket");
    assert_eq!(WebSearchConfig::default().provider_id(), "soket");
    // The default round-trips to a minimal object.
    assert_eq!(
        serde_json::to_value(WebSearchConfig::default()).unwrap(),
        json!({"enabled": true, "allowPrivateAddresses": false})
    );
    let fetch: WebFetchConfig = serde_json::from_str(r#"{"provider":"direct"}"#).unwrap();
    assert_eq!(fetch.provider, WebFetchProvider::Direct);
}

#[tokio::test]
async fn current_api_search_keeps_snippets_and_legacy_descriptions() {
    let server = FakeHttpServer::always(FakeResponse::json(json!({
        "query": "Soket AI", "results": [
            {"title":"Current","url":"https://example.com/current","snippet":"Current API excerpt", "description":"Old excerpt"},
            {"title":"Legacy","url":"https://example.com/legacy","description":"Legacy API excerpt"},
            {"title":"Empty snippet","url":"https://example.com/fallback","snippet":" ","description":"Fallback excerpt"}
        ]
    }).to_string()));
    let tool = create_web_search_tool(search_config(&server, None), Some("sk-test")).unwrap();
    let result = execute(&tool, json!({"query":"Soket AI", "max_results":3}))
        .await
        .unwrap();
    let output = text(&result);
    assert!(output.contains("Current API excerpt"));
    assert!(!output.contains("Old excerpt"));
    assert!(output.contains("Legacy API excerpt"));
    assert!(output.contains("Fallback excerpt"));
}

#[tokio::test]
async fn current_api_fetch_keeps_page_content_and_upstream_truncation() {
    let server = FakeHttpServer::always(FakeResponse::json(
        json!({
            "url":"https://example.com/", "title":"Example Domain",
            "content":"Useful page body beyond the title.", "truncated":true
        })
        .to_string(),
    ));
    let tool = create_web_fetch_tool(soket_fetch_config(), Some(soket_api(&server))).unwrap();
    let result = execute(&tool, json!({"url":"https://example.com"}))
        .await
        .unwrap();
    assert_eq!(
        text(&result),
        "# Example Domain\n\nUseful page body beyond the title."
    );
    assert_eq!(
        result.details["bytes"],
        "Useful page body beyond the title.".len()
    );
    assert_eq!(result.details["truncated"], true);
}

#[tokio::test]
async fn soket_fetch_does_not_report_title_only_output_as_page_content() {
    let server = FakeHttpServer::always(FakeResponse::json(
        r#"{"url":"https://example.com/","title":"Heading only","content":""}"#,
    ));
    let tool = create_web_fetch_tool(soket_fetch_config(), Some(soket_api(&server))).unwrap();
    let error = execute(&tool, json!({"url":"https://example.com"}))
        .await
        .unwrap_err();
    assert!(error.contains("no page content"), "{error}");
}

#[tokio::test]
#[ignore = "requires LOOP_TEST_SOKET_WEB=1, a Soket API key, and network access"]
async fn live_soket_web_responses_include_body_text() {
    if std::env::var("LOOP_TEST_SOKET_WEB").as_deref() != Ok("1") {
        return;
    }
    let api = SoketWebApi::resolve(None).expect("set SOKET_API_KEY for the live smoke check");
    let search = create_web_search_tool(WebSearchConfig::default(), Some(&api.api_key)).unwrap();
    let result = execute(&search, json!({"query":"Soket AI India", "max_results":2}))
        .await
        .unwrap();
    let hits = result.details["results"]
        .as_array()
        .expect("search results");
    assert!(!hits.is_empty());
    assert!(
        hits.iter().any(|hit| hit["snippet"]
            .as_str()
            .is_some_and(|snippet| !snippet.trim().is_empty())),
        "live search must retain result snippets"
    );
    let fetch = create_web_fetch_tool(WebFetchConfig::default(), Some(api)).unwrap();
    let result = execute(&fetch, json!({"url":"https://example.com"}))
        .await
        .unwrap();
    assert!(
        text(&result).contains("documentation examples"),
        "live fetch must retain the page body"
    );
    assert!(result.details["bytes"].as_u64().unwrap() > 100);
}
