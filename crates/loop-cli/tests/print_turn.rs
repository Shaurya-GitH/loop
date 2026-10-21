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
    backend_with(responses, with_model, vec![]).await.0
}

async fn backend_with(
    responses: Vec<FauxResponse>,
    with_model: bool,
    tools: Vec<loop_agent::AgentTool>,
) -> (Arc<NativeBackend>, Arc<AgentHarness>) {
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
        tools,
        system_prompt: "sys".into(),
        sandbox: SandboxMode::Disabled,
        resources: Default::default(),
    }));
    (NativeBackend::new(Arc::clone(&harness)).await, harness)
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

fn recording_bash() -> (loop_agent::AgentTool, Arc<std::sync::atomic::AtomicBool>) {
    let ran = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let flag = Arc::clone(&ran);
    let bash = loop_agent::AgentTool::simple(
        "bash",
        "bash",
        "run a command",
        json!({"type": "object", "properties": {"command": {"type": "string"}}}),
        move |_, _, _, _| {
            let flag = Arc::clone(&flag);
            async move {
                flag.store(true, std::sync::atomic::Ordering::SeqCst);
                Ok(loop_agent::AgentToolResult::text("ran"))
            }
        },
    );
    (bash, ran)
}

fn bash_call(command: &str) -> Vec<FauxResponse> {
    vec![
        FauxResponse::ToolCalls(vec![ToolCall {
            id: "c1".into(),
            name: "bash".into(),
            arguments: json!({ "command": command }),
            thought_signature: None,
        }]),
        FauxResponse::Text("done".into()),
    ]
}

/// `--print` has no approver, but deny rules still hold.
#[tokio::test]
async fn denied_tools_do_not_run_in_print_mode() {
    let (bash, ran) = recording_bash();
    let (backend, harness) = backend_with(bash_call("ls"), true, vec![bash]).await;
    let gate = loop_app_core::tool_gate::ToolGate::new(
        loop_app_core::tool_approval::permissions_from_settings(&std::collections::BTreeMap::from(
            [("bash".into(), "deny".into())],
        )),
    );
    gate.install(&harness);
    let session = backend.current_session().await;
    let text = print_turn(backend.as_ref(), &session, "go".into())
        .await
        .unwrap();
    assert_eq!(text, "done");
    assert!(
        !ran.load(std::sync::atomic::Ordering::SeqCst),
        "a denied tool must not run"
    );
}

/// A `tool_call` hook blocks a matching call; the gate runs the harness's
/// hooks once installed.
#[tokio::test]
async fn hooks_block_matching_calls_in_print_mode() {
    let dir = tempfile::tempdir().unwrap();
    let hook = dir.path().join("no-force.json");
    std::fs::write(
        &hook,
        json!({"on": "tool_call", "match": {"args": {"command": "*--force*"}},
               "action": {"block": true}})
        .to_string(),
    )
    .unwrap();
    for (command, should_run) in [("git push --force", false), ("git status", true)] {
        let (bash, ran) = recording_bash();
        let (backend, harness) = backend_with(bash_call(command), true, vec![bash]).await;
        let notices =
            loop_app_core::hooks_load::register_json_hooks(&harness, std::slice::from_ref(&hook));
        assert!(notices.is_empty(), "{notices:?}");
        loop_app_core::tool_gate::ToolGate::new(Default::default()).install(&harness);
        let session = backend.current_session().await;
        print_turn(backend.as_ref(), &session, "go".into())
            .await
            .unwrap();
        assert_eq!(
            ran.load(std::sync::atomic::Ordering::SeqCst),
            should_run,
            "{command}"
        );
    }
}

/// Nobody can approve in `--print`: in `ask` mode a tool that needs approval
/// is refused, and `full-access` runs it.
#[tokio::test]
async fn print_mode_refuses_what_needs_approval_unless_full_access() {
    use loop_app_core::permissions::PermissionMode;
    for (mode, should_run) in [
        (PermissionMode::Ask, false),
        (PermissionMode::ReadOnly, false),
        (PermissionMode::AcceptEdits, false),
        (PermissionMode::FullAccess, true),
    ] {
        let (bash, ran) = recording_bash();
        let (backend, harness) = backend_with(bash_call("ls"), true, vec![bash]).await;
        let gate = loop_app_core::tool_gate::ToolGate::new(
            loop_app_core::tool_approval::permissions_from_settings(&Default::default()),
        );
        gate.set_mode(mode);
        gate.install(&harness);
        let session = backend.current_session().await;
        print_turn(backend.as_ref(), &session, "go".into())
            .await
            .unwrap();
        assert_eq!(
            ran.load(std::sync::atomic::Ordering::SeqCst),
            should_run,
            "{mode:?}"
        );
    }
}
