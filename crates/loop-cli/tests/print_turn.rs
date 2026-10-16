//! `--print` reads its answer from the backend's update stream.

use std::sync::Arc;

use loop_agent::harness::{
    create_in_memory_session_store, create_session_repository, AgentHarness, AgentHarnessOptions,
    ExecutionEnv, HostExecutionEnv, SandboxMode,
};
use loop_agent::native_backend::NativeBackend;
use loop_ai::providers::{faux_provider, FauxResponse, FauxScript};
use loop_ai::{Models, ToolCall};
use loop_cli::print_mode::print_turn;
use serde_json::json;

async fn backend(responses: Vec<FauxResponse>, with_model: bool) -> Arc<NativeBackend> {
    let script = FauxScript::new();
    script.extend(responses);
    let models = Arc::new(Models::new());
    models.set_provider(faux_provider(script));
    let model = with_model.then(|| models.get_model("faux", "faux-model").unwrap());
    let repo = create_session_repository(create_in_memory_session_store(), None);
    let session = repo.create(None, None).await.unwrap();
    let host: Arc<dyn ExecutionEnv> = Arc::new(HostExecutionEnv::new(std::env::temp_dir()));
    let harness = Arc::new(AgentHarness::new(AgentHarnessOptions {
        models,
        model,
        session,
        host_env: host,
        tools: vec![],
        system_prompt: "sys".into(),
        sandbox: SandboxMode::Disabled,
        resources: Default::default(),
    }));
    NativeBackend::new(harness).await
}

#[tokio::test]
async fn prints_the_answer() {
    let backend = backend(vec![FauxResponse::Text("hello".into())], true).await;
    let session = backend.current_session().await;
    let text = print_turn(backend.as_ref(), &session, "hi".into())
        .await
        .unwrap();
    assert_eq!(text, "hello");
}

#[tokio::test]
async fn prints_only_the_last_message_of_a_tool_turn() {
    let backend = backend(
        vec![
            FauxResponse::ToolCalls(vec![ToolCall {
                id: "c1".into(),
                name: "missing_tool".into(),
                arguments: json!({}),
                thought_signature: None,
            }]),
            FauxResponse::Text("final answer".into()),
        ],
        true,
    )
    .await;
    let session = backend.current_session().await;
    let text = print_turn(backend.as_ref(), &session, "go".into())
        .await
        .unwrap();
    assert_eq!(text, "final answer");
}

#[tokio::test]
async fn provider_failure_is_an_error() {
    let backend = backend(
        vec![FauxResponse::Error("HTTP 502 Bad Gateway".into())],
        true,
    )
    .await;
    let session = backend.current_session().await;
    let err = print_turn(backend.as_ref(), &session, "hi".into())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("502"), "{err}");
}

/// The harness refuses before the agent loop starts; the backend still
/// closes the turn on the stream, so print mode does not hang.
#[tokio::test]
async fn no_model_is_an_error_not_a_hang() {
    let backend = backend(vec![], false).await;
    let session = backend.current_session().await;
    let err = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        print_turn(backend.as_ref(), &session, "hi".into()),
    )
    .await
    .expect("print_turn ends")
    .unwrap_err();
    assert!(err.to_string().to_lowercase().contains("model"), "{err}");
}
