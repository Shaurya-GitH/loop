//! `/tracing` state persists to the agent dir (separate test binary: mutates process env).

use loop_cli::config::paths::{auth_path, settings_path, ENV_AGENT_DIR, ENV_SESSION_DIR};
use loop_cli::config::settings::Settings;
use loop_cli::config::TracingSetupRequest;
use loop_cli::{bootstrap_cli, BootstrapOpts};
use loop_telemetry::{CredentialSource, TelemetryHandle};

#[tokio::test]
async fn tracing_commands_persist_to_settings_and_auth() {
    let agent_dir = tempfile::tempdir().unwrap();
    let cwd = tempfile::tempdir().unwrap();
    // SAFETY: only test in this binary; no concurrent env access.
    unsafe {
        std::env::set_var(ENV_AGENT_DIR, agent_dir.path());
        std::env::set_var(ENV_SESSION_DIR, agent_dir.path().join("sessions"));
        // Non-interactive bootstrap needs a provider key; no model is called.
        std::env::set_var("LOOP_API_KEY", "test-key");
        for key in [
            "LANGFUSE_HOST",
            "LANGFUSE_PUBLIC_KEY",
            "LANGFUSE_SECRET_KEY",
        ] {
            std::env::remove_var(key);
        }
    }
    let mut runtime = bootstrap_cli(BootstrapOpts {
        cwd: cwd.path().to_path_buf(),
        provider: None,
        model: None,
        theme: None,
        system_prompt: None,
        append_system_prompt: None,
        no_context_files: true,
        interactive: false,
        session_id: None,
        agent: None,
    })
    .await
    .unwrap();

    let status = runtime
        .attach_telemetry(TelemetryHandle::new("test", true))
        .unwrap();
    assert!(status.destination.is_none() && !status.active());

    let status = runtime.set_tracing_enabled(false).unwrap();
    assert!(!status.enabled);
    let saved = Settings::load_file(&settings_path(agent_dir.path())).unwrap();
    assert!(!saved.tracing.enabled);

    let status = runtime
        .setup_tracing(&TracingSetupRequest::Langfuse {
            host: "https://lf.example".into(),
            public_key: "pk-lf-1".into(),
            secret_key: "sk-lf-secret".into(),
        })
        .unwrap();
    assert!(status.active());
    assert_eq!(status.source, Some(CredentialSource::Config));

    let settings_json = std::fs::read_to_string(settings_path(agent_dir.path())).unwrap();
    let saved: Settings = serde_json::from_str(&settings_json).unwrap();
    assert!(saved.tracing.enabled);
    assert_eq!(
        saved.tracing.langfuse_host.as_deref(),
        Some("https://lf.example")
    );
    assert_eq!(
        saved.tracing.langfuse_public_key.as_deref(),
        Some("pk-lf-1")
    );
    assert!(!settings_json.contains("sk-lf-secret"));
    let auth_json = std::fs::read_to_string(auth_path(agent_dir.path())).unwrap();
    assert!(auth_json.contains("sk-lf-secret"));

    // A fresh runtime picks the saved credentials back up.
    let mut again = bootstrap_cli(BootstrapOpts {
        cwd: cwd.path().to_path_buf(),
        provider: None,
        model: None,
        theme: None,
        system_prompt: None,
        append_system_prompt: None,
        no_context_files: true,
        interactive: false,
        session_id: None,
        agent: None,
    })
    .await
    .unwrap();
    let status = again
        .attach_telemetry(TelemetryHandle::new("test", true))
        .unwrap();
    assert_eq!(
        status.destination.as_deref(),
        Some("Langfuse · https://lf.example")
    );
    assert!(status.active());
}
