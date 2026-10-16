//! NativeBackend: Loop's own agent behind the AgentBackend interface.

use std::sync::Arc;
use std::time::Duration;

use loop_agent::harness::{
    create_in_memory_session_store, create_session_repository, AgentHarness, AgentHarnessOptions,
    ExecutionEnv, HostExecutionEnv, SandboxMode,
};
use loop_agent::native_backend::NativeBackend;
use loop_agent::{AgentTool, AgentToolResult};
use loop_ai::providers::{faux_provider, FauxResponse, FauxScript};
use loop_ai::{Models, ToolCall};
use loop_protocol::{
    AgentBackend, BackendError, ContentBlock, ErrorCategory, SessionKey, SessionRef, SessionSpec,
    SessionUpdate, StopReason, TurnOutcome, UpdateEnvelope,
};
use serde_json::json;
use tokio::sync::broadcast;

async fn backend(responses: Vec<FauxResponse>, tools: Vec<AgentTool>) -> Arc<NativeBackend> {
    NativeBackend::new(harness(responses, tools).await).await
}

async fn harness(responses: Vec<FauxResponse>, tools: Vec<AgentTool>) -> Arc<AgentHarness> {
    let script = FauxScript::new();
    script.extend(responses);
    let models = Arc::new(Models::new());
    models.set_provider(faux_provider(script));
    let model = models.get_model("faux", "faux-model").unwrap();
    let repo = create_session_repository(create_in_memory_session_store(), None);
    let session = repo.create(None, None).await.unwrap();
    let host: Arc<dyn ExecutionEnv> = Arc::new(HostExecutionEnv::new(std::env::temp_dir()));
    Arc::new(AgentHarness::new(AgentHarnessOptions {
        models,
        model: Some(model),
        session,
        host_env: host,
        tools,
        system_prompt: "sys".into(),
        sandbox: SandboxMode::Disabled,
        resources: Default::default(),
    }))
}

/// Envelopes until (and including) the turn's end.
async fn collect_turn(rx: &mut broadcast::Receiver<UpdateEnvelope>) -> Vec<UpdateEnvelope> {
    let mut out = Vec::new();
    loop {
        let envelope = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("turn ends")
            .unwrap();
        let end = matches!(envelope.update, SessionUpdate::TurnEnded { .. });
        out.push(envelope);
        if end {
            return out;
        }
    }
}

fn text(s: &str) -> Vec<ContentBlock> {
    vec![ContentBlock::text(s)]
}

#[tokio::test]
async fn prompt_streams_envelopes_and_resolves_once() {
    let backend = backend(vec![FauxResponse::Text("hello".into())], vec![]).await;
    let session = backend.current_session().await;
    let mut rx = backend.subscribe();

    let handle = backend.prompt(&session, text("hi")).await.unwrap();
    let envelopes = collect_turn(&mut rx).await;
    assert_eq!(
        handle.outcome().await,
        TurnOutcome::Completed(StopReason::EndTurn)
    );

    let mut last_seq = 0;
    for envelope in &envelopes {
        assert_eq!(envelope.instance.as_str(), "loop");
        assert_eq!(envelope.session, session.key);
        assert_eq!(envelope.generation, 0);
        assert!(envelope.seq > last_seq, "seq strictly increases");
        last_seq = envelope.seq;
    }
    let turn_id = envelopes
        .iter()
        .find_map(|e| match &e.update {
            SessionUpdate::TurnStarted { turn_id } => Some(turn_id.clone()),
            _ => None,
        })
        .unwrap();
    for envelope in envelopes
        .iter()
        .skip_while(|e| !matches!(e.update, SessionUpdate::TurnStarted { .. }))
    {
        assert_eq!(envelope.turn.as_ref(), Some(&turn_id), "{envelope:?}");
    }
}

#[tokio::test]
async fn failed_turn_outcome_carries_category() {
    let backend = backend(vec![FauxResponse::Error("HTTP 401 bad key".into())], vec![]).await;
    let session = backend.current_session().await;
    let handle = backend.prompt(&session, text("hi")).await.unwrap();
    match handle.outcome().await {
        TurnOutcome::Failed(info) => assert_eq!(info.category, ErrorCategory::Auth),
        other => panic!("expected failure, got {other:?}"),
    }
}

#[tokio::test]
async fn second_prompt_while_busy_is_rejected() {
    let wait = AgentTool::simple(
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
    let backend = backend(
        vec![
            FauxResponse::ToolCalls(vec![ToolCall {
                id: "w1".into(),
                name: "wait".into(),
                arguments: json!({}),
                thought_signature: None,
            }]),
            FauxResponse::Text("after".into()),
        ],
        vec![wait],
    )
    .await;
    let session = backend.current_session().await;
    let mut rx = backend.subscribe();
    let handle = backend.prompt(&session, text("wait")).await.unwrap();
    // Wait until the tool is running.
    loop {
        let envelope = rx.recv().await.unwrap();
        if matches!(envelope.update, SessionUpdate::ToolCall(_)) {
            break;
        }
    }
    assert!(backend.is_busy(&session));
    assert_eq!(
        backend.prompt(&session, text("again")).await.unwrap_err(),
        BackendError::Busy
    );

    backend.cancel(&session);
    tokio::time::timeout(Duration::from_secs(5), handle.outcome())
        .await
        .expect("cancel ends the turn");
}

#[tokio::test]
async fn unknown_session_is_rejected() {
    let backend = backend(vec![], vec![]).await;
    let stranger = SessionRef {
        instance: "loop".into(),
        key: SessionKey::from("nope"),
        native_id: "nope".into(),
    };
    assert!(matches!(
        backend.prompt(&stranger, text("hi")).await,
        Err(BackendError::SessionNotFound(_))
    ));
}

#[tokio::test]
async fn new_session_moves_the_stream() {
    let backend = backend(vec![FauxResponse::Text("x".into())], vec![]).await;
    let first = backend.current_session().await;
    let second = backend.new_session(SessionSpec::default()).await.unwrap();
    assert_ne!(first.key, second.key);
    assert_eq!(backend.current_session().await, second);

    let mut rx = backend.subscribe();
    let handle = backend.prompt(&second, text("hi")).await.unwrap();
    let envelopes = collect_turn(&mut rx).await;
    handle.outcome().await;
    assert!(envelopes.iter().all(|e| e.session == second.key));
    assert_eq!(envelopes[0].seq, 1, "sequence restarts per session");
}

#[tokio::test]
async fn services_are_available() {
    let backend = backend(vec![FauxResponse::Text("x".into())], vec![]).await;
    let session = backend.current_session().await;
    backend
        .prompt(&session, text("hi"))
        .await
        .unwrap()
        .outcome()
        .await;

    let stats = backend
        .session_stats()
        .unwrap()
        .stats(&session)
        .await
        .unwrap();
    assert!(!stats.report.is_empty());

    let shell = backend
        .local_shell()
        .unwrap()
        .run(&session, "echo native-shell")
        .await
        .unwrap();
    assert_eq!(shell.exit_code, 0);
    assert!(shell.stdout.contains("native-shell"));

    // One short exchange leaves nothing to compact; the harness's refusal
    // comes back as a typed failure rather than a panic or a silent success.
    let refused = backend
        .compaction()
        .unwrap()
        .compact(&session, None)
        .await
        .unwrap_err();
    assert!(
        refused.to_string().contains("nothing to compact"),
        "{refused}"
    );
}

#[tokio::test]
async fn back_to_back_prompts_cannot_both_start() {
    let backend = backend(vec![FauxResponse::Text("one".into())], Vec::new()).await;
    let session = backend.current_session().await;
    let mut rx = backend.subscribe();
    // Before the first prompt's task has run, the harness still looks idle.
    let first = backend.prompt(&session, text("one")).await.unwrap();
    assert!(backend.is_busy(&session));
    assert!(matches!(
        backend.prompt(&session, text("two")).await,
        Err(BackendError::Busy)
    ));
    assert_eq!(
        first.outcome().await,
        TurnOutcome::Completed(StopReason::EndTurn)
    );
    collect_turn(&mut rx).await;
    assert!(!backend.is_busy(&session));
}

#[tokio::test]
async fn a_busy_rejection_leaves_the_running_turn_open() {
    let harness = harness(
        vec![
            FauxResponse::Text("direct".into()),
            FauxResponse::Text("unused".into()),
        ],
        Vec::new(),
    )
    .await;
    let backend = NativeBackend::new(Arc::clone(&harness)).await;
    let session = backend.current_session().await;
    let mut rx = backend.subscribe();
    // The backend's check passes, then another caller takes the harness
    // before the backend's prompt task runs.
    let queued = backend.prompt(&session, text("queued")).await.unwrap();
    let mut direct = Box::pin(harness.prompt("direct"));
    assert!(futures::poll!(&mut direct).is_pending());

    match queued.outcome().await {
        TurnOutcome::Failed(info) => assert_eq!(info.message, "a turn is already running"),
        other => panic!("expected a busy failure, got {other:?}"),
    }
    direct.await.unwrap();
    let updates: Vec<_> = collect_turn(&mut rx)
        .await
        .into_iter()
        .map(|e| e.update)
        .collect();
    assert!(updates.iter().any(|u| matches!(u,
        SessionUpdate::Notice { text, .. } if text == "a turn is already running")));
    let ends: Vec<_> = updates
        .iter()
        .filter_map(|u| match u {
            SessionUpdate::TurnEnded { stop, .. } => Some(*stop),
            _ => None,
        })
        .collect();
    assert_eq!(
        ends,
        [StopReason::EndTurn],
        "only the direct turn ends, normally"
    );
}

#[tokio::test]
async fn a_provider_error_is_reported_once() {
    let backend = backend(
        vec![FauxResponse::Error("HTTP 400 bad request".into())],
        vec![],
    )
    .await;
    let session = backend.current_session().await;
    let mut rx = backend.subscribe();
    let handle = backend.prompt(&session, text("hi")).await.unwrap();
    assert!(matches!(handle.outcome().await, TurnOutcome::Failed(_)));
    let mut updates: Vec<_> = collect_turn(&mut rx)
        .await
        .into_iter()
        .map(|e| e.update)
        .collect();
    // Anything sent after the turn's end.
    while let Ok(Ok(envelope)) = tokio::time::timeout(Duration::from_millis(100), rx.recv()).await {
        updates.push(envelope.update);
    }
    let reports = updates
        .iter()
        .filter(|u| {
            matches!(u, SessionUpdate::TurnEnded { error: Some(_), .. })
                || matches!(
                    u,
                    SessionUpdate::Notice {
                        level: loop_protocol::NoticeLevel::Error,
                        ..
                    }
                )
        })
        .count();
    assert_eq!(reports, 1, "{updates:?}");
}
