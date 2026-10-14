//! Parity tests: pin how `AgentHarness` behaves today, as seen by the
//! surfaces (event order, the returned message, what is persisted).
//!
//! The AgentBackend refactor (docs/design/backend-inventory.md) must keep
//! every assertion here passing. A deliberate behavior change updates the
//! expectation in the same PR and says so.

use std::sync::Arc;
use std::time::Duration;

use loop_agent::harness::{
    create_in_memory_session_store, create_session_repository, create_write_tool, AgentHarness,
    AgentHarnessOptions, ExecutionEnv, HostExecutionEnv, SandboxMode,
};
use loop_agent::{AgentEvent, AgentMessage, AgentTool, AgentToolResult, BeforeToolCallResult};
use loop_ai::providers::{faux_provider, FauxResponse, FauxScript};
use loop_ai::{Models, StopReason, ToolCall};
use parking_lot::Mutex;
use serde_json::json;

struct Fixture {
    harness: Arc<AgentHarness>,
    events: Arc<Mutex<Vec<AgentEvent>>>,
    dir: tempfile::TempDir,
}

impl Fixture {
    async fn new(responses: Vec<FauxResponse>, extra_tools: Vec<AgentTool>) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let script = FauxScript::new();
        script.extend(responses);
        let models = Arc::new(Models::new());
        models.set_provider(faux_provider(script));
        let model = models.get_model("faux", "faux-model").unwrap();
        let repo = create_session_repository(create_in_memory_session_store(), None);
        let session = repo.create(None, None).await.unwrap();
        let host: Arc<dyn ExecutionEnv> = Arc::new(HostExecutionEnv::new(dir.path().to_path_buf()));
        let mut tools = vec![create_write_tool(Arc::clone(&host))];
        tools.extend(extra_tools);
        let harness = Arc::new(AgentHarness::new(AgentHarnessOptions {
            models,
            model: Some(model),
            session,
            host_env: host,
            tools,
            system_prompt: "sys".into(),
            sandbox: SandboxMode::Disabled,
            resources: Default::default(),
        }));
        let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::default();
        let sink = Arc::clone(&events);
        harness.subscribe(move |event| {
            let sink = Arc::clone(&sink);
            async move { sink.lock().push(event) }
        });
        Self {
            harness,
            events,
            dir,
        }
    }

    /// Event names in order, without progress notices (their wording and
    /// count are presentation details, not contract).
    fn event_names(&self) -> Vec<&'static str> {
        self.events
            .lock()
            .iter()
            .filter(|e| !matches!(e, AgentEvent::Progress { .. }))
            .map(AgentEvent::type_name)
            .collect()
    }

    async fn persisted_roles(&self) -> Vec<&'static str> {
        self.harness
            .session_context()
            .await
            .unwrap()
            .messages
            .iter()
            .map(AgentMessage::role)
            .collect()
    }
}

fn write_call(id: &str, path: &str) -> ToolCall {
    ToolCall {
        id: id.into(),
        name: "write".into(),
        arguments: json!({"path": path, "content": "hi\n"}),
        thought_signature: None,
    }
}

#[tokio::test]
async fn text_turn() {
    let fx = Fixture::new(vec![FauxResponse::Text("hello".into())], vec![]).await;
    let message = fx.harness.prompt("hi").await.unwrap();
    fx.harness.wait_for_idle().await;

    let assistant = message
        .as_assistant()
        .expect("prompt returns the assistant message");
    assert_eq!(assistant.stop_reason, StopReason::Stop);
    assert_eq!(
        fx.event_names(),
        [
            "agent_start",
            "turn_start",
            "message_start", // user prompt
            "message_end",
            "message_start", // assistant
            "message_update",
            "message_update",
            "message_update",
            "message_end",
            "turn_end",
            "agent_end",
        ]
    );
    assert_eq!(fx.persisted_roles().await, ["user", "assistant"]);
}

#[tokio::test]
async fn tool_turn() {
    let fx = Fixture::new(
        vec![
            FauxResponse::ToolCalls(vec![write_call("c1", "a.txt")]),
            FauxResponse::Text("done".into()),
        ],
        vec![],
    )
    .await;
    let message = fx.harness.prompt("write a.txt").await.unwrap();
    fx.harness.wait_for_idle().await;

    assert_eq!(
        message.as_assistant().unwrap().stop_reason,
        StopReason::Stop
    );
    assert!(fx.dir.path().join("a.txt").exists());
    let names = fx.event_names();
    let pos = |name: &str| names.iter().position(|n| *n == name).unwrap();
    assert!(pos("tool_execution_start") < pos("tool_execution_end"));
    assert_eq!(names.iter().filter(|n| **n == "turn_end").count(), 2);
    assert_eq!(names.last(), Some(&"agent_end"));
    assert_eq!(
        fx.persisted_roles().await,
        ["user", "assistant", "toolResult", "assistant"]
    );
}

#[tokio::test]
async fn provider_error_is_returned_not_raised() {
    let fx = Fixture::new(vec![FauxResponse::Error("HTTP 500 boom".into())], vec![]).await;
    let message = fx.harness.prompt("hi").await.unwrap();
    fx.harness.wait_for_idle().await;

    let assistant = message.as_assistant().unwrap();
    assert_eq!(assistant.stop_reason, StopReason::Error);
    assert!(assistant.failure().unwrap().contains("boom"));
    assert_eq!(fx.event_names().last(), Some(&"agent_end"));
    assert_eq!(fx.persisted_roles().await, ["user", "assistant"]);
}

#[tokio::test]
async fn before_tool_hook_blocks_the_call() {
    let fx = Fixture::new(
        vec![
            FauxResponse::ToolCalls(vec![write_call("c1", "blocked.txt")]),
            FauxResponse::Text("ok".into()),
        ],
        vec![],
    )
    .await;
    fx.harness
        .set_before_tool_call(Some(Arc::new(|_ctx, _cancel| {
            Box::pin(async {
                Some(BeforeToolCallResult {
                    block: true,
                    reason: Some("denied by test".into()),
                })
            })
        })));
    fx.harness.prompt("write").await.unwrap();
    fx.harness.wait_for_idle().await;

    assert!(!fx.dir.path().join("blocked.txt").exists());
    let blocked = fx.events.lock().iter().any(|e| {
        matches!(e, AgentEvent::ToolExecutionEnd { is_error: true, result, .. }
            if format!("{:?}", result.content).contains("denied by test"))
    });
    assert!(blocked, "blocked call ends as an error carrying the reason");
}

/// Current behavior: a cancelled turn persists nothing from that turn, not
/// even the prompt. The session-records work (roadmap P0) will change this;
/// update this test there, not in the backend refactor.
#[tokio::test]
async fn abort_during_tool_ends_turn_and_persists_nothing() {
    let wait_tool = AgentTool::simple(
        "wait",
        "Wait",
        "Waits until cancelled.",
        json!({"type": "object", "properties": {}}),
        |_id, _args, cancel, _update| async move {
            if let Some(cancel) = cancel {
                tokio::select! {
                    _ = cancel.cancelled() => {}
                    _ = tokio::time::sleep(Duration::from_secs(10)) => {}
                }
            }
            Ok(AgentToolResult::text("waited"))
        },
    );
    let fx = Fixture::new(
        vec![
            FauxResponse::ToolCalls(vec![ToolCall {
                id: "w1".into(),
                name: "wait".into(),
                arguments: json!({}),
                thought_signature: None,
            }]),
            FauxResponse::Text("unreachable".into()),
        ],
        vec![wait_tool],
    )
    .await;

    let harness = Arc::clone(&fx.harness);
    let running = tokio::spawn(async move { harness.prompt("wait").await });
    for _ in 0..200 {
        if fx.event_names().contains(&"tool_execution_start") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    fx.harness.abort();
    tokio::time::timeout(Duration::from_secs(5), running)
        .await
        .expect("abort ends the turn promptly")
        .unwrap()
        .unwrap();
    fx.harness.wait_for_idle().await;

    // The loop does not check cancellation before its next model call; a
    // real provider aborts that request through the token, the faux provider
    // answers anyway. So the final stop reason is provider-dependent and is
    // not pinned here. What is pinned: the turn ends, and nothing persists.
    assert_eq!(fx.event_names().last(), Some(&"agent_end"));
    assert!(fx.events.lock().iter().any(
        |e| matches!(e, AgentEvent::ToolExecutionEnd { tool_call_id, .. } if tool_call_id == "w1")
    ));
    assert!(fx.persisted_roles().await.is_empty());
}
