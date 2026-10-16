use loop_agent::harness::{create_web_fetch_tool, WebFetchConfig, WebFetchProvider};
use loop_ai::ToolResultContent;
use loop_test_support::{FakeHttpServer, FakeResponse};
use serde_json::json;

#[tokio::test]
async fn direct_fetch_ignores_environment_proxies() {
    let target = FakeHttpServer::always(FakeResponse::new(
        200,
        "text/html",
        "<html><title>Direct</title><p>Fetched from target</p></html>",
    ));
    let proxy = FakeHttpServer::always(FakeResponse::new(
        200,
        "text/html",
        "<html><title>Proxy</title><p>Fetched from proxy</p></html>",
    ));

    let proxy_url = proxy.base_url();
    let names = [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
    ];
    let no_proxy_names = ["NO_PROXY", "no_proxy"];
    let previous: Vec<_> = names
        .iter()
        .map(|name| (name, std::env::var_os(name)))
        .collect();
    let previous_no_proxy: Vec<_> = no_proxy_names
        .iter()
        .map(|name| (name, std::env::var_os(name)))
        .collect();
    for name in names {
        std::env::set_var(name, proxy_url);
    }
    for name in no_proxy_names {
        std::env::set_var(name, "");
    }

    let tool = create_web_fetch_tool(
        WebFetchConfig {
            allow_private_addresses: true,
            provider: WebFetchProvider::Direct,
        },
        None,
    )
    .expect("direct fetch tool");

    for (name, value) in previous {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }
    for (name, value) in previous_no_proxy {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }

    let result = (tool.execute)("test".into(), json!({"url": target.base_url()}), None, None)
        .await
        .expect("direct target fetch succeeds");

    assert_eq!(target.requests().len(), 1);
    assert!(proxy.requests().is_empty(), "request reached proxy");
    match &result.content[0] {
        ToolResultContent::Text(text) => assert!(text.text.contains("Fetched from target")),
        ToolResultContent::Image(_) => panic!("unexpected image result"),
    }
}
