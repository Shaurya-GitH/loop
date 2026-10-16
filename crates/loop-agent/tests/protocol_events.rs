//! Loop's own agent, driven by the faux provider, translated into protocol
//! updates. These pin the update sequence surfaces will render.

use std::sync::Arc;

use loop_agent::harness::{
    create_in_memory_session_store, create_session_repository, create_write_tool, AgentHarness,
    AgentHarnessOptions, ExecutionEnv, HostExecutionEnv, SandboxMode,
};
use loop_agent::protocol_events::NativeEventMapper;
use loop_agent::AgentEvent;
use loop_ai::providers::{faux_provider, FauxResponse, FauxScript};
use loop_ai::{Models, ToolCall};
use loop_protocol::{
    ContentBlock, ErrorCategory, Role, SessionUpdate, StopReason, ToolCallStatus, ToolContent,
    ToolKind,
};
use parking_lot::Mutex;
use serde_json::json;

async fn run(
    responses: Vec<FauxResponse>,
    prompt: &str,
) -> (Vec<SessionUpdate>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let script = FauxScript::new();
    script.extend(responses);
    let models = Arc::new(Models::new());
    models.set_provider(faux_provider(script));
    let model = models.get_model("faux", "faux-model").unwrap();
    let repo = create_session_repository(create_in_memory_session_store(), None);
    let session = repo.create(None, None).await.unwrap();
    let host: Arc<dyn ExecutionEnv> = Arc::new(HostExecutionEnv::new(dir.path().to_path_buf()));

    let harness = AgentHarness::new(AgentHarnessOptions {
        models,
        model: Some(model),
        session,
        host_env: Arc::clone(&host),
        tools: vec![create_write_tool(host)],
        system_prompt: "sys".into(),
        sandbox: SandboxMode::Disabled,
        resources: Default::default(),
    });
    let events: Arc<Mutex<Vec<AgentEvent>>> = Arc::default();
    let sink = Arc::clone(&events);
    harness.subscribe(move |event| {
        let sink = Arc::clone(&sink);
        async move { sink.lock().push(event) }
    });

    let _ = harness.prompt(prompt).await;
    harness.wait_for_idle().await;

    let mut mapper = NativeEventMapper::new();
    let updates = events.lock().iter().flat_map(|e| mapper.map(e)).collect();
    (updates, dir)
}

fn agent_text(updates: &[SessionUpdate]) -> String {
    updates
        .iter()
        .filter_map(|u| match u {
            SessionUpdate::MessageChunk {
                role: Role::Agent,
                content: ContentBlock::Text { text },
                ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn turn_end(updates: &[SessionUpdate]) -> &SessionUpdate {
    let ends: Vec<_> = updates
        .iter()
        .filter(|u| matches!(u, SessionUpdate::TurnEnded { .. }))
        .collect();
    assert_eq!(ends.len(), 1, "exactly one TurnEnded: {updates:#?}");
    assert!(
        matches!(updates.last(), Some(SessionUpdate::TurnEnded { .. })),
        "TurnEnded is last: {updates:#?}"
    );
    ends[0]
}

#[tokio::test]
async fn text_turn() {
    let (updates, _dir) = run(vec![FauxResponse::Text("hello there".into())], "hi").await;

    // Progress notices may come before the turn; nothing else does.
    let first = updates
        .iter()
        .find(|u| !matches!(u, SessionUpdate::Notice { .. }));
    assert!(matches!(first, Some(SessionUpdate::TurnStarted { .. })));
    assert_eq!(agent_text(&updates), "hello there");

    // The user's prompt is a complete message of its own.
    let user_chunk = updates.iter().find_map(|u| match u {
        SessionUpdate::MessageChunk {
            role: Role::User,
            content,
            ..
        } => content.as_text(),
        _ => None,
    });
    assert_eq!(user_chunk, Some("hi"));

    // Every started message is ended, in order, before the turn ends.
    let mut open = Vec::new();
    for update in &updates {
        match update {
            SessionUpdate::MessageStarted { message_id, .. } => open.push(message_id.clone()),
            SessionUpdate::MessageEnded { message_id } => {
                assert_eq!(open.pop().as_ref(), Some(message_id));
            }
            SessionUpdate::MessageChunk { message_id, .. } => {
                assert_eq!(open.last(), Some(message_id), "chunk inside its message");
            }
            _ => {}
        }
    }
    assert!(open.is_empty());

    assert!(matches!(
        turn_end(&updates),
        SessionUpdate::TurnEnded {
            stop: StopReason::EndTurn,
            error: None,
            ..
        }
    ));
}

#[tokio::test]
async fn write_tool_reports_kind_location_and_diff() {
    let call = ToolCall {
        id: "call-1".into(),
        name: "write".into(),
        arguments: json!({"path": "hello.txt", "content": "hi\n"}),
        thought_signature: None,
    };
    let (updates, dir) = run(
        vec![
            FauxResponse::ToolCalls(vec![call]),
            FauxResponse::Text("Created hello.txt.".into()),
        ],
        "create hello.txt",
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(dir.path().join("hello.txt")).unwrap(),
        "hi\n"
    );

    let started = updates
        .iter()
        .find_map(|u| match u {
            SessionUpdate::ToolCall(call) => Some(call),
            _ => None,
        })
        .expect("tool call reported");
    assert_eq!(started.id.as_str(), "call-1");
    assert_eq!(started.kind, ToolKind::Edit);
    assert_eq!(started.title, "write hello.txt");
    assert_eq!(started.status, ToolCallStatus::InProgress);
    assert_eq!(started.locations[0].path, "hello.txt");

    let finished = updates
        .iter()
        .find_map(|u| match u {
            SessionUpdate::ToolCallUpdate(update)
                if update.status == Some(ToolCallStatus::Completed) =>
            {
                Some(update)
            }
            _ => None,
        })
        .expect("tool call completed");
    let content = finished.content.as_ref().unwrap();
    assert!(
        matches!(&content[0], ToolContent::UnifiedDiff { path, diff } if path.ends_with("hello.txt") && diff.contains("+hi")),
        "diff first: {content:#?}"
    );

    assert_eq!(agent_text(&updates), "Created hello.txt.");
    assert!(matches!(
        turn_end(&updates),
        SessionUpdate::TurnEnded {
            stop: StopReason::EndTurn,
            ..
        }
    ));
}

#[tokio::test]
async fn failed_turn_has_category() {
    let (updates, _dir) = run(
        vec![FauxResponse::Error("HTTP 429 Too Many Requests".into())],
        "hi",
    )
    .await;
    match turn_end(&updates) {
        SessionUpdate::TurnEnded {
            stop: StopReason::Error,
            error: Some(error),
            ..
        } => {
            assert_eq!(error.category, ErrorCategory::RateLimit);
            assert!(error.retryable);
        }
        other => panic!("expected failed turn, got {other:?}"),
    }
}

#[tokio::test]
async fn turn_ids_increase_per_run() {
    let mut mapper = NativeEventMapper::new();
    let first = mapper.map(&AgentEvent::AgentStart);
    mapper.map(&AgentEvent::AgentEnd { messages: vec![] });
    let second = mapper.map(&AgentEvent::AgentStart);
    let id = |u: &[SessionUpdate]| match &u[0] {
        SessionUpdate::TurnStarted { turn_id } => turn_id.clone(),
        other => panic!("{other:?}"),
    };
    assert_ne!(id(&first), id(&second));
    assert!(mapper.current_turn().is_some());
}
