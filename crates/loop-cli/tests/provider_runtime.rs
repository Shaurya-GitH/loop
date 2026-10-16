//! `/login` / `/logout` persistence against local fake providers (separate test binary:
//! it mutates process env).

use loop_cli::config::paths::{auth_path, settings_path, ENV_AGENT_DIR, ENV_SESSION_DIR};
use loop_cli::config::ProviderLoginRequest;
use loop_cli::{bootstrap_cli, BootstrapOpts, CliRuntime};
use loop_test_support::{FakeHttpServer, FakeResponse};

const MODELS: &str = r#"{"data":[{"id":"m-one"},{"id":"m-two"}]}"#;

fn models_server() -> FakeHttpServer {
    FakeHttpServer::start(|req| match req.path.as_str() {
        "/v1/models" => FakeResponse::json(MODELS),
        _ => FakeResponse::new(404, "text/plain", "nope"),
    })
}

async fn boot(cwd: &std::path::Path) -> CliRuntime {
    bootstrap_cli(BootstrapOpts {
        cwd: cwd.to_path_buf(),
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

fn read(path: std::path::PathBuf) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

#[tokio::test]
async fn connect_reject_restart_and_disconnect() {
    let agent_dir = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    let existing = models_server();
    // A saved custom provider counts as connected, so non-interactive bootstrap works
    // without any real provider key.
    std::fs::write(
        settings_path(agent_dir.path()),
        format!(
            r#"{{"providers":[{{"id":"existing","name":"Existing","baseUrl":"{}/v1"}}]}}"#,
            existing.base_url()
        ),
    )
    .unwrap();
    // SAFETY: only test in this binary; no concurrent env access.
    unsafe {
        std::env::set_var(ENV_AGENT_DIR, agent_dir.path());
        std::env::set_var(ENV_SESSION_DIR, agent_dir.path().join("sessions"));
        for key in [
            "SOKET_API_KEY",
            "TENSORSTUDIO_API_KEY",
            "LOOP_API_KEY",
            "OPENROUTER_API_KEY",
            "OPENAI_API_KEY",
        ] {
            std::env::remove_var(key);
        }
    }

    let mut runtime = boot(cwd.path()).await;
    assert!(!runtime.needs_provider_setup);
    assert_eq!(runtime.connected_providers(), ["existing"]);

    // Connect a keyed custom provider.
    let gateway = models_server();
    let connected = runtime
        .connect_provider(&ProviderLoginRequest::Custom {
            name: "Gateway".into(),
            base_url: format!("{}/v1", gateway.base_url()),
            api_key: Some("k-secret".into()),
        })
        .await
        .unwrap();
    assert_eq!(
        (connected.id.as_str(), connected.model_count),
        ("gateway", 2)
    );
    assert_eq!(
        gateway.requests()[0].header("authorization"),
        Some("Bearer k-secret")
    );
    assert!(read(settings_path(agent_dir.path())).contains(r#""id": "gateway""#));
    assert!(!read(settings_path(agent_dir.path())).contains("k-secret"));
    assert!(read(auth_path(agent_dir.path())).contains("k-secret"));

    // Picker order: providers alphabetically (no Soket key here), models by id.
    let order: Vec<String> = runtime
        .available_models()
        .await
        .iter()
        .map(|m| format!("{}/{}", m.provider, m.id))
        .collect();
    assert_eq!(
        order,
        [
            "existing/m-one",
            "existing/m-two",
            "gateway/m-one",
            "gateway/m-two"
        ]
    );

    // A rejected key leaves nothing behind.
    let rejecting = FakeHttpServer::always(FakeResponse::new(
        401,
        "application/json",
        r#"{"error":{"message":"bad key"}}"#,
    ));
    let err = runtime
        .connect_provider(&ProviderLoginRequest::Custom {
            name: "Broken".into(),
            base_url: format!("{}/v1", rejecting.base_url()),
            api_key: Some("k-bad".into()),
        })
        .await
        .unwrap_err()
        .to_string();
    assert!(err.contains("401"), "{err}");
    assert!(!read(auth_path(agent_dir.path())).contains("k-bad"));
    assert!(!read(settings_path(agent_dir.path())).contains("broken"));
    assert!(runtime.models.get_provider("broken").is_none());

    // Nothing is selected until the user picks a model.
    assert!(runtime.selected_model.is_none());
    assert!(runtime.harness.model().await.is_none());
    let model = runtime.select_model("gateway", "m-two").await.unwrap();
    assert_eq!(model.id, "m-two");
    assert_eq!(
        runtime.selected_model_spec().as_deref(),
        Some("gateway/m-two")
    );
    assert_eq!(runtime.harness.model().await.unwrap().id, "m-two");
    assert!(runtime.select_model("gateway", "nope").await.is_err());

    // Restart: the connected provider, its key and the selection are still there.
    drop(runtime);
    let mut runtime = boot(cwd.path()).await;
    assert_eq!(runtime.connected_providers(), ["existing", "gateway"]);
    assert_eq!(
        runtime.selected_model_spec().as_deref(),
        Some("gateway/m-two")
    );

    // Disconnect removes entry, key and provider.
    assert_eq!(
        runtime.disconnect_provider("gateway").await.unwrap(),
        "Gateway"
    );
    assert!(runtime.selected_model.is_none(), "its model is deselected");
    assert!(runtime.harness.model().await.is_none());
    assert_eq!(
        runtime.model_note.as_deref(),
        Some("gateway/m-two was disconnected")
    );
    assert_eq!(runtime.connected_providers(), ["existing"]);
    assert!(runtime.models.get_provider("gateway").is_none());
    assert!(!read(auth_path(agent_dir.path())).contains("k-secret"));
    assert!(!read(settings_path(agent_dir.path())).contains("gateway"));
    assert!(runtime.disconnect_provider("gateway").await.is_err());
}
