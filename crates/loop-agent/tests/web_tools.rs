//! Integration tests for the `web_fetch` / `web_search` tools.
//!
//! All HTTP traffic is served by the loop-test-support fake server on
//! 127.0.0.1 — which is exactly what the default (strict) SSRF policy must
//! refuse to talk to. Tests that need to exercise the happy path opt into
//! `allowPrivateAddresses`, mirroring the documented intranet-mirror setting.

use loop_agent::harness::{create_web_fetch_tool, create_web_search_tool, verify_web_search_key};
use loop_agent::types::AgentTool;
use loop_test_support::{FakeHttpServer, FakeResponse};
use serde_json::json;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// `AgentTool` is not `Debug`, so `expect_err` is unavailable.
fn expect_creation_error(result: Result<AgentTool, String>, context: &str) -> String {
    match result {
        Ok(_) => panic!("{context}: expected an error"),
        Err(error) => error,
    }
}

/// A `web_fetch` tool that may talk to the fake server on 127.0.0.1
/// (the documented intranet-mirror opt-in).
fn fetch_tool_allowing_private_hosts() -> AgentTool {
    create_web_fetch_tool(
        loop_agent::harness::WebFetchConfig {
            allow_private_addresses: true,
            ..Default::default()
        },
        None,
    )
    .expect("web_fetch tool")
}

async fn execute_tool(
    tool: &AgentTool,
    args: serde_json::Value,
) -> Result<loop_agent::types::AgentToolResult, String> {
    (tool.execute)("test".into(), args, None, None).await
}

/// The text content of a tool result (empty when the result has none).
fn result_text(result: &loop_agent::types::AgentToolResult) -> String {
    match result.content.first() {
        Some(loop_ai::ToolResultContent::Text(text)) => text.text.clone(),
        _ => String::new(),
    }
}

#[tokio::test]
async fn fetch_converts_html_to_text() {
    let server = FakeHttpServer::always(FakeResponse::new(
        200,
        "text/html; charset=utf-8",
        r#"<html><head><script>alert(1)</script></head>
           <body><h1>Hello</h1><p>World &amp; more</p><ul><li>a</li><li>b</li></ul></body></html>"#,
    ));
    let tool = fetch_tool_allowing_private_hosts();
    let result = execute_tool(&tool, json!({"url": format!("{}/page", server.base_url())}))
        .await
        .expect("fetch ok");
    let text = result_text(&result);
    assert!(text.contains("Hello"));
    assert!(text.contains("World & more"));
    assert!(text.contains("- a"));
    assert!(!text.contains("alert"), "script bodies must be dropped");
    assert_eq!(result.details["status"], 200);
    assert_eq!(result.details["contentType"], "text/html");
}

#[tokio::test]
async fn fetch_passes_json_through() {
    let server = FakeHttpServer::always(FakeResponse::json(
        r#"{"models": ["m1", "m2"], "object": "list"}"#,
    ));
    let tool = fetch_tool_allowing_private_hosts();
    let result = execute_tool(&tool, json!({"url": server.base_url()}))
        .await
        .expect("fetch ok");
    assert_eq!(
        result_text(&result),
        r#"{"models": ["m1", "m2"], "object": "list"}"#
    );
}

#[tokio::test]
async fn fetch_follows_redirects_and_reports_origin() {
    let server = FakeHttpServer::start(|req| match req.path.as_str() {
        "/start" => FakeResponse::new(302, "text/plain", "").with_header("Location", "/final"),
        "/final" => FakeResponse::new(200, "text/plain", "landed"),
        _ => FakeResponse::new(404, "text/plain", "nope"),
    });
    let tool = fetch_tool_allowing_private_hosts();
    let result = execute_tool(
        &tool,
        json!({"url": format!("{}/start", server.base_url())}),
    )
    .await
    .expect("fetch ok");
    assert_eq!(result_text(&result), "landed");
    assert_eq!(
        result.details["url"],
        json!(format!("{}/final", server.base_url()))
    );
    assert_eq!(
        result.details["redirectedFrom"],
        json!(format!("{}/start", server.base_url()))
    );
    // Two requests: the redirect hop and the target.
    assert_eq!(server.requests().len(), 2);
}

#[tokio::test]
async fn fetch_reports_http_errors() {
    let server = FakeHttpServer::always(FakeResponse::new(404, "text/plain", "missing"));
    let tool = fetch_tool_allowing_private_hosts();
    let err = execute_tool(&tool, json!({"url": server.base_url()}))
        .await
        .expect_err("404 must be an error");
    assert!(err.contains("404"), "unexpected error: {err}");
}

#[tokio::test]
async fn fetch_cancellation_interrupts_a_stalled_body_chunk() {
    let server = FakeHttpServer::always(
        FakeResponse::new(200, "text/plain", "body").with_body_delay(Duration::from_secs(30)),
    );
    let tool = fetch_tool_allowing_private_hosts();
    let url = format!("{}/page", server.base_url());
    let cancel = CancellationToken::new();
    let task_cancel = cancel.clone();
    let task = tokio::spawn(async move {
        (tool.execute)("test".into(), json!({"url": url}), Some(task_cancel), None).await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while server.requests().is_empty() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("request reaches fake server");
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .expect("cancellation interrupts the pending body read")
        .expect("fetch task joins");
    assert!(result.unwrap_err().contains("cancelled"));
}

#[tokio::test]
async fn fetch_rejects_private_addresses_by_default() {
    let server = FakeHttpServer::always(FakeResponse::new(200, "text/plain", "secret"));
    let tool = create_web_fetch_tool(Default::default(), None).expect("web_fetch tool");
    let err = execute_tool(&tool, json!({"url": server.base_url()}))
        .await
        .expect_err("strict policy must refuse loopback");
    assert!(err.contains("refusing"), "unexpected error: {err}");
    // Nothing should have reached the server.
    assert!(server.requests().is_empty());
}

#[tokio::test]
async fn fetch_rejects_non_http_schemes_and_credentials() {
    let tool = fetch_tool_allowing_private_hosts();
    for url in [
        "file:///etc/passwd",
        "ftp://example.com/pub",
        "https://user:pass@example.com/",
    ] {
        let err = match execute_tool(&tool, json!({"url": url})).await {
            Err(err) => err,
            Ok(_) => panic!("{url} must be rejected"),
        };
        assert!(!err.is_empty(), "{url} must produce an error message");
    }
}

#[tokio::test]
async fn fetch_truncates_large_bodies() {
    let body = "word ".repeat(40_000); // 200 KB > the 50 KB model cap
    let server = FakeHttpServer::always(FakeResponse::new(200, "text/plain", body));
    let tool = fetch_tool_allowing_private_hosts();
    let result = execute_tool(&tool, json!({"url": server.base_url()}))
        .await
        .expect("fetch ok");
    assert_eq!(result.details["truncated"], true);
    assert!(result_text(&result).len() <= 50_000);
}

#[tokio::test]
async fn fetch_rejects_unsupported_content_types() {
    let server = FakeHttpServer::always(FakeResponse::new(200, "image/png", vec![1, 2, 3]));
    let tool = fetch_tool_allowing_private_hosts();
    let err = execute_tool(&tool, json!({"url": server.base_url()}))
        .await
        .expect_err("images must be rejected");
    assert!(err.contains("image/png"), "unexpected error: {err}");
}

fn search_config(
    server: &FakeHttpServer,
    key_env: &str,
    provider: &str,
) -> loop_agent::harness::WebSearchConfig {
    loop_agent::harness::WebSearchConfig {
        provider: Some(provider.into()),
        api_key_env: vec![key_env.into()],
        base_url: Some(server.base_url().to_string()),
        allow_private_addresses: true,
        ..Default::default()
    }
}

#[tokio::test]
async fn search_tavily_backend() {
    std::env::set_var("LOOP_TEST_TAVILY_KEY", "tvly-test");
    let server = FakeHttpServer::always(FakeResponse::json(
        r#"{"results":[
            {"title": "Rust", "url": "https://rust-lang.org", "content": "Systems language"},
            {"title": "Docs", "url": "https://doc.rust-lang.org", "content": "Book"}
        ]}"#,
    ));
    let tool = create_web_search_tool(
        search_config(&server, "LOOP_TEST_TAVILY_KEY", "tavily"),
        None,
    )
    .expect("web_search tool");
    let result = execute_tool(&tool, json!({"query": "rust language", "max_results": 2}))
        .await
        .expect("search ok");
    let text = result_text(&result);
    assert!(text.contains("1. Rust"));
    assert!(text.contains("https://rust-lang.org"));
    assert!(text.contains("Systems language"));

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "POST");
    assert_eq!(requests[0].path, "/search");
    assert_eq!(
        requests[0].header("authorization"),
        Some("Bearer tvly-test")
    );
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["query"], "rust language");
    assert_eq!(body["max_results"], 2);
    assert_eq!(result.details["provider"], "tavily");
}

#[tokio::test]
async fn search_brave_backend() {
    std::env::set_var("LOOP_TEST_BRAVE_KEY", "brv-test");
    let server = FakeHttpServer::always(FakeResponse::json(
        r#"{"web":{"results":[
            {"title": "Rust", "url": "https://rust-lang.org", "description": "Systems language"}
        ]}}"#,
    ));
    let tool = create_web_search_tool(search_config(&server, "LOOP_TEST_BRAVE_KEY", "brave"), None)
        .expect("web_search tool");
    let result = execute_tool(&tool, json!({"query": "rust"}))
        .await
        .expect("search ok");
    assert!(result_text(&result).contains("https://rust-lang.org"));

    let requests = server.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].method, "GET");
    assert!(requests[0].path.starts_with("/res/v1/web/search?"));
    assert!(requests[0].path.contains("q=rust"));
    assert!(requests[0].path.contains("count=5"));
    assert_eq!(requests[0].header("x-subscription-token"), Some("brv-test"));
}

#[tokio::test]
async fn search_requires_an_api_key() {
    std::env::remove_var("LOOP_TEST_MISSING_KEY");
    let err = expect_creation_error(
        create_web_search_tool(
            loop_agent::harness::WebSearchConfig {
                provider: Some("tavily".into()),
                api_key_env: vec!["LOOP_TEST_MISSING_KEY".into()],
                ..Default::default()
            },
            None,
        ),
        "missing key must fail tool creation",
    );
    assert!(err.contains("API key"), "unexpected error: {err}");
}

#[tokio::test]
async fn search_rejects_unknown_provider() {
    std::env::set_var("LOOP_TEST_ANY_KEY", "k");
    let err = expect_creation_error(
        create_web_search_tool(
            loop_agent::harness::WebSearchConfig {
                provider: Some("altavista".into()),
                api_key_env: vec!["LOOP_TEST_ANY_KEY".into()],
                ..Default::default()
            },
            None,
        ),
        "unknown provider must fail tool creation",
    );
    assert!(err.contains("altavista"), "unexpected error: {err}");
}

#[tokio::test]
async fn search_rejects_private_base_urls_by_default() {
    let server = FakeHttpServer::always(FakeResponse::json("{}"));
    std::env::set_var("LOOP_TEST_STRICT_KEY", "k");
    let err = expect_creation_error(
        create_web_search_tool(
            loop_agent::harness::WebSearchConfig {
                provider: Some("tavily".into()),
                api_key_env: vec!["LOOP_TEST_STRICT_KEY".into()],
                base_url: Some(server.base_url().to_string()),
                ..Default::default()
            },
            None,
        ),
        "strict policy must refuse loopback base URL",
    );
    assert!(err.contains("refusing"), "unexpected error: {err}");
}

#[tokio::test]
async fn search_clamps_max_results() {
    std::env::set_var("LOOP_TEST_CLAMP_KEY", "k");
    let server = FakeHttpServer::always(FakeResponse::json(
        r#"{"results":[{"title":"a","url":"https://a","content":"c"}]}"#,
    ));
    let tool = create_web_search_tool(
        search_config(&server, "LOOP_TEST_CLAMP_KEY", "tavily"),
        None,
    )
    .expect("web_search tool");
    execute_tool(&tool, json!({"query": "x", "max_results": 99}))
        .await
        .expect("search ok");
    let body: serde_json::Value = serde_json::from_slice(&server.requests()[0].body).unwrap();
    assert_eq!(body["max_results"], 10);
}

#[tokio::test]
async fn search_uses_the_stored_login_key() {
    std::env::remove_var("TAVILY_API_KEY");
    std::env::remove_var("LOOP_TEST_STORED_KEY");
    let server = FakeHttpServer::always(FakeResponse::json(
        r#"{"results":[{"title":"t","url":"https://t","content":"c"}]}"#,
    ));
    // No env keys configured at all — the key comes from the `/login` store.
    let mut config = search_config(&server, "LOOP_TEST_STORED_KEY", "tavily");
    config.api_key_env = vec![];
    let tool = create_web_search_tool(config, Some("tvly-stored")).expect("web_search tool");
    execute_tool(&tool, json!({"query": "q"}))
        .await
        .expect("search ok");
    assert_eq!(
        server.requests()[0].header("authorization"),
        Some("Bearer tvly-stored")
    );
}

#[tokio::test]
async fn search_env_key_overrides_the_stored_key() {
    std::env::set_var("LOOP_TEST_OVERRIDE_KEY", "tvly-from-env");
    let server = FakeHttpServer::always(FakeResponse::json(
        r#"{"results":[{"title":"t","url":"https://t","content":"c"}]}"#,
    ));
    let tool = create_web_search_tool(
        search_config(&server, "LOOP_TEST_OVERRIDE_KEY", "tavily"),
        Some("tvly-stored"),
    )
    .expect("web_search tool");
    execute_tool(&tool, json!({"query": "q"}))
        .await
        .expect("search ok");
    assert_eq!(
        server.requests()[0].header("authorization"),
        Some("Bearer tvly-from-env")
    );
}

#[tokio::test]
async fn key_verification_probes_and_reports_rejections() {
    let ok = FakeHttpServer::always(FakeResponse::json(
        r#"{"results":[{"title":"t","url":"https://t","content":"c"}]}"#,
    ));
    let config = search_config(&ok, "LOOP_TEST_VERIFY_KEY", "tavily");
    verify_web_search_key(&config, "tvly-good")
        .await
        .expect("key accepted");

    let rejected = FakeHttpServer::always(FakeResponse::new(401, "application/json", "{}"));
    let config = search_config(&rejected, "LOOP_TEST_VERIFY_KEY", "tavily");
    let err = verify_web_search_key(&config, "tvly-bad")
        .await
        .expect_err("401 must fail verification");
    assert!(err.contains("rejected"), "unexpected error: {err}");
}

#[tokio::test]
async fn fetch_rejects_redirects_to_other_schemes() {
    let server = FakeHttpServer::always(
        FakeResponse::new(302, "text/plain", "").with_header("Location", "ftp://example.com/file"),
    );
    let tool = fetch_tool_allowing_private_hosts();
    let err = execute_tool(&tool, json!({"url": server.base_url()}))
        .await
        .expect_err("non-http redirect targets must be rejected");
    assert!(err.contains("scheme"), "unexpected error: {err}");
    // Only the first (rejected) hop was made.
    assert_eq!(server.requests().len(), 1);
}

#[tokio::test]
async fn fetch_stops_redirect_loops() {
    let server = FakeHttpServer::start(|request| match request.path.as_str() {
        "/a" => FakeResponse::new(302, "text/plain", "").with_header("Location", "/b"),
        _ => FakeResponse::new(302, "text/plain", "").with_header("Location", "/a"),
    });
    let tool = fetch_tool_allowing_private_hosts();
    let err = execute_tool(&tool, json!({"url": format!("{}/a", server.base_url())}))
        .await
        .expect_err("a redirect loop must terminate with an error");
    assert!(
        err.contains("too many redirects"),
        "unexpected error: {err}"
    );
    // 1 initial request + 5 followed redirects.
    assert_eq!(server.requests().len(), 6);
}

#[tokio::test]
async fn fetch_strips_utf8_byte_order_mark() {
    let server = FakeHttpServer::always(FakeResponse::new(
        200,
        "text/plain",
        "\u{feff}hello".to_string().into_bytes(),
    ));
    let tool = fetch_tool_allowing_private_hosts();
    let result = execute_tool(&tool, json!({"url": server.base_url()}))
        .await
        .expect("fetch ok");
    assert_eq!(result_text(&result), "hello");
}

#[tokio::test]
async fn fetch_handles_empty_bodies() {
    let server = FakeHttpServer::always(FakeResponse::new(200, "text/plain", ""));
    let tool = fetch_tool_allowing_private_hosts();
    let result = execute_tool(&tool, json!({"url": server.base_url()}))
        .await
        .expect("fetch ok");
    assert_eq!(result_text(&result), "");
    assert_eq!(result.details["bytes"], 0);
    assert_eq!(result.details["truncated"], false);
}

#[tokio::test]
async fn fetch_treats_empty_content_type_as_plain_text() {
    let server = FakeHttpServer::always(FakeResponse::new(200, "", "raw words"));
    let tool = fetch_tool_allowing_private_hosts();
    let result = execute_tool(&tool, json!({"url": server.base_url()}))
        .await
        .expect("fetch ok");
    assert_eq!(result_text(&result), "raw words");
    assert_eq!(result.details["contentType"], "text/plain");
}
