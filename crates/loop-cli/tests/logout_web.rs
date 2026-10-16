//! Isolated process: environment credentials must not bypass explicit logout.
use loop_ai::{Credential, CredentialStore};
use loop_cli::config::paths::{ENV_AGENT_DIR, ENV_SESSION_DIR};
use loop_cli::config::ProviderLoginRequest;
use loop_cli::{bootstrap_cli, BootstrapOpts, CliRuntime};
use loop_test_support::{FakeHttpServer, FakeResponse};
use serde_json::json;

async fn boot(cwd: &std::path::Path) -> CliRuntime {
    bootstrap_cli(BootstrapOpts {
        cwd: cwd.into(),
        provider: None,
        model: None,
        theme: None,
        system_prompt: None,
        append_system_prompt: None,
        no_context_files: true,
        no_web: false,
        interactive: false,
        session_id: None,
        agent: None,
    })
    .await
    .unwrap()
}

#[tokio::test]
async fn logout_blocks_soket_keys_until_verified_login() {
    let agent_dir = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let server =
        FakeHttpServer::start(
            |request| match (request.method.as_str(), request.path.as_str()) {
                ("GET", "/v1/models")
                    if request.header("authorization") == Some("Bearer rejected") =>
                {
                    FakeResponse::new(401, "text/plain", "rejected key")
                }
                ("GET", "/v1/models") => FakeResponse::json(r#"{"data":[{"id":"local-model"}]}"#),
                ("POST", "/v1/search") => FakeResponse::json(r#"{"ok":true,"results":[]}"#),
                ("GET", "/page") => FakeResponse::new(200, "text/plain", "direct fetch works"),
                _ => FakeResponse::new(500, "text/plain", "unexpected request"),
            },
        );
    // Override the model endpoints so this test cannot contact real providers.
    std::fs::write(agent_dir.path().join("models.json"), json!({"providers":[
        {"id":"soket","baseUrl":format!("{}/v1", server.base_url()),"apiKeyEnv":["SOKET_API_KEY"],"models":["local-model"]}
    ]}).to_string()).unwrap();
    std::fs::write(agent_dir.path().join("settings.json"), json!({
        "providers":[{"id":"local","name":"Local","baseUrl":format!("{}/v1", server.base_url())}],
        "webSearch":{"apiKeyEnv":["LOOP_TEST_LOGOUT_CUSTOM"],"baseUrl":format!("{}/v1", server.base_url()),"allowPrivateAddresses":true}
    }).to_string()).unwrap();
    // SAFETY: this integration-test binary has one test. All keys are fake.
    unsafe {
        std::env::set_var(ENV_AGENT_DIR, agent_dir.path());
        std::env::set_var(ENV_SESSION_DIR, agent_dir.path().join("sessions"));
        for name in [
            "SOKET_API_KEY",
            "TENSORSTUDIO_API_KEY",
            "LOOP_API_KEY",
            "LOOP_TEST_LOGOUT_CUSTOM",
        ] {
            std::env::set_var(name, "ambient-test-key");
        }
        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("OPENROUTER_API_KEY");
    }
    let mut runtime = boot(cwd.path()).await;
    let model = runtime.select_model("soket", "local-model").await.unwrap();
    assert!(runtime
        .available_models()
        .await
        .iter()
        .any(|m| m.provider == "soket"));
    assert!(runtime
        .web_tools()
        .build()
        .iter()
        .any(|t| t.name == "web_search"));
    // No saved Soket credential: logout must still work and deselect its model.
    assert!(runtime.credentials.get("soket").is_none());
    assert_eq!(runtime.logout_provider("SoKeT").await.unwrap(), "Soket");
    assert!(runtime.credentials.is_logged_out("soket"));
    assert!(runtime.selected_model.is_none());
    assert!(runtime.harness.model().await.is_none());
    assert!(!runtime.connected_providers().iter().any(|p| p == "soket"));
    assert!(!runtime
        .available_models()
        .await
        .iter()
        .any(|m| m.provider == "soket"));
    assert!(runtime.select_model("soket", &model.id).await.is_err());
    let provider = runtime.models.get_provider("soket").unwrap();
    assert!(loop_ai::auth::resolve_provider_auth(
        "soket",
        &provider.auth,
        runtime.credentials.as_ref(),
        Some("explicit-test-key")
    )
    .await
    .is_err());
    assert!(!loop_cli::config::auth::provider_has_key(
        runtime.credentials.as_ref(),
        "soket",
        &["SOKET_API_KEY"]
    ));
    // A forced catalog refresh must not contact a logged-out provider.
    let original_provider = runtime.models.get_provider("soket").unwrap();
    runtime
        .models
        .set_provider(loop_ai::providers::custom_provider(
            loop_ai::providers::CustomProviderConfig {
                id: "soket".into(),
                name: None,
                base_url: format!("{}/blocked/v1", server.base_url()),
                api_key_env: vec!["SOKET_API_KEY".into()],
                models: vec![],
                headers: None,
            },
        ));
    runtime
        .models
        .refresh(loop_ai::ModelsRefreshOptions {
            provider_id: Some("soket".into()),
            allow_network: Some(true),
            force: true,
        })
        .await;
    assert!(!server
        .requests()
        .iter()
        .any(|r| r.path.starts_with("/blocked/")));
    runtime.models.set_provider(original_provider);
    runtime.set_web_enabled(true).await.unwrap();
    assert!(!runtime
        .harness
        .system_prompt()
        .await
        .contains("- web_search:"));
    assert!(runtime
        .harness
        .system_prompt()
        .await
        .contains("- web_fetch:"));
    assert!(runtime.web_status().contains("search: Soket unavailable"));
    assert!(runtime.web_status().contains("direct"));
    runtime.settings.web_fetch = Some(loop_agent::harness::WebFetchConfig {
        provider: loop_agent::harness::WebFetchProvider::Soket,
        ..Default::default()
    });
    assert!(runtime.web_tools().build().is_empty());
    assert!(runtime.web_status().contains("web_fetch unavailable"));
    runtime.settings.web_fetch = Some(loop_agent::harness::WebFetchConfig {
        allow_private_addresses: true,
        ..Default::default()
    });
    let fetch = runtime.web_tools().build().remove(0);
    let result = (fetch.execute)(
        "fetch-test".into(),
        json!({"url":format!("{}/page",server.base_url())}),
        None,
        None,
    )
    .await
    .unwrap();
    assert!(format!("{:?}", result.content).contains("direct fetch works"));
    // A separate backend and the subscription's tools survive Soket logout.
    runtime
        .credentials
        .connect("tavily", Some(Credential::api_key("tavily-test")))
        .unwrap();
    runtime.set_web_provider("tavily").await.unwrap();
    assert!(runtime
        .web_tools()
        .build()
        .iter()
        .any(|t| t.name == "web_search"));
    runtime.active_agent = Some("chatgpt".into());
    assert!(runtime.web_status().contains("ChatGPT uses its own tools"));
    runtime.active_agent = None;
    runtime.logout_provider("tavily").await.unwrap();
    assert_eq!(
        runtime.settings.web_search.as_ref().unwrap().provider_id(),
        "tavily"
    );
    assert!(!runtime
        .web_tools()
        .build()
        .iter()
        .any(|t| t.name == "web_search"));
    runtime.set_web_provider("soket").await.unwrap();
    // A restart with environment keys and a stale saved model cannot reconnect.
    runtime.settings.set_selected_model("soket", "local-model");
    runtime.save_settings().unwrap();
    drop(runtime);
    let mut runtime = boot(cwd.path()).await;
    assert!(runtime.selected_model.is_none());
    assert!(runtime.credentials.is_logged_out("soket"));
    assert!(!runtime
        .available_models()
        .await
        .iter()
        .any(|m| m.provider == "soket"));
    // Rejected login leaves the persisted logout unchanged, even with cached models.
    let rejected = ProviderLoginRequest::Preset {
        id: "soket".into(),
        api_key: "rejected".into(),
    };
    assert!(runtime.connect_provider(&rejected).await.is_err());
    assert!(runtime.credentials.is_logged_out("soket"));
    assert!(runtime.credentials.get("soket").is_none());
    let verified = ProviderLoginRequest::Preset {
        id: "soket".into(),
        api_key: "verified-test-key".into(),
    };
    let blocked_write = runtime.credentials.path().with_extension("json.tmp");
    std::fs::create_dir(&blocked_write).unwrap();
    assert!(runtime.connect_provider(&verified).await.is_err());
    assert!(runtime.credentials.is_logged_out("soket"));
    assert!(runtime.credentials.get("soket").is_none());
    std::fs::remove_dir(&blocked_write).unwrap();
    runtime.connect_provider(&verified).await.unwrap();
    assert!(!runtime.credentials.is_logged_out("soket"));
    assert_eq!(
        runtime.credentials.get("soket"),
        Some(Credential::api_key("verified-test-key"))
    );
    assert!(runtime
        .available_models()
        .await
        .iter()
        .any(|m| m.provider == "soket"));
    assert!(runtime
        .web_tools()
        .build()
        .iter()
        .any(|t| t.name == "web_search"));
    // Saved and ambient keys together are also blocked, across store reopen.
    runtime.logout_provider("soket").await.unwrap();
    let reopened = loop_cli::config::FileCredentialStore::open(runtime.credentials.path()).unwrap();
    assert!(reopened.is_logged_out("soket"));
    assert!(reopened.get("soket").is_none());
    let search_count = server
        .requests()
        .iter()
        .filter(|r| r.path.ends_with("/search"))
        .count();
    assert_eq!(search_count, 0, "logout must not probe a search backend");
}
