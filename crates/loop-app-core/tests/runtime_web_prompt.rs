use loop_app_core::{bootstrap, BootstrapOpts};

#[tokio::test]
async fn websearch_toggle_refreshes_the_system_prompt() {
    let agent_dir = tempfile::tempdir().unwrap();
    let sessions = agent_dir.path().join("sessions");
    let cwd = tempfile::tempdir().unwrap();
    // SAFETY: this integration-test binary contains one test and the process
    // exits after it, so no concurrent test reads these variables.
    unsafe {
        std::env::set_var("LOOP_CODING_AGENT_DIR", agent_dir.path());
        std::env::set_var("LOOP_CODING_AGENT_SESSION_DIR", sessions);
        std::env::set_var("OPENAI_API_KEY", "test-model-key");
        std::env::set_var("SOKET_API_KEY", "test-soket-key");
    }

    let mut runtime = bootstrap(BootstrapOpts {
        cwd: cwd.path().to_path_buf(),
        provider: None,
        model: None,
        theme: None,
        system_prompt: None,
        append_system_prompt: None,
        no_context_files: true,
        interactive: false,
        session_id: None,
        no_web: false,
        agent: None,
    })
    .await
    .expect("runtime bootstraps with test credentials");

    let initial = runtime.harness.system_prompt().await;
    assert!(initial.contains("- web_fetch:"));
    assert!(initial.contains("- web_search:"));
    assert!(initial.contains("Discover unfamiliar URLs with web_search"));

    runtime.set_web_enabled(false).await.unwrap();
    let disabled = runtime.harness.system_prompt().await;
    assert!(!disabled.contains("web_fetch"));
    assert!(!disabled.contains("web_search"));

    runtime.set_web_enabled(true).await.unwrap();
    let enabled = runtime.harness.system_prompt().await;
    assert!(enabled.contains("- web_fetch:"));
    assert!(enabled.contains("- web_search:"));
}
