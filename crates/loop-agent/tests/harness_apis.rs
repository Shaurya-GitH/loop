//! AgentHarness extended API tests.

use std::path::PathBuf;
use std::sync::Arc;

use loop_agent::harness::hooks::{HarnessHookEvent, HookOutcome};
use loop_agent::harness::{
    create_in_memory_session_store, create_session_repository, AgentHarness, AgentHarnessOptions,
    AgentHarnessResources, CompactionSettings, HostExecutionEnv, PromptTemplate, SandboxMode,
    Skill,
};
use loop_agent::AgentMessage;
use loop_ai::providers::{faux_provider, FauxResponse, FauxScript};
use loop_ai::Models;

async fn make_harness(script: FauxScript, resources: AgentHarnessResources) -> AgentHarness {
    let models = Arc::new(Models::new());
    models.set_provider(faux_provider(script));
    let model = models.get_model("faux", "faux-model").unwrap();

    let store = create_in_memory_session_store();
    let repo = create_session_repository(store, None);
    let session = repo.create(None, Some("apis".into())).await.unwrap();
    let host = Arc::new(HostExecutionEnv::new(std::env::temp_dir()));

    AgentHarness::new(AgentHarnessOptions {
        models,
        model: Some(model),
        session,
        host_env: host,
        tools: vec![],
        system_prompt: "sys".into(),
        sandbox: SandboxMode::Disabled,
        resources,
    })
}

#[tokio::test]
async fn skill_prompts_with_faux() {
    let script = FauxScript::new();
    script.push(FauxResponse::Text("skill-done".into()));

    let resources = AgentHarnessResources {
        skills: vec![Skill {
            name: "demo".into(),
            description: "A demo".into(),
            body: "Do the thing.".into(),
            path: PathBuf::from("/tmp/SKILL.md"),
            disable_model_invocation: false,
        }],
        ..Default::default()
    };

    let harness = make_harness(script, resources).await;
    let msg = harness.skill("demo", Some("extra args")).await.unwrap();
    assert_eq!(msg.role(), "assistant");
}

#[tokio::test]
async fn prompt_from_template_with_resources() {
    let script = FauxScript::new();
    script.push(FauxResponse::Text("tpl-done".into()));

    let resources = AgentHarnessResources {
        prompt_templates: vec![PromptTemplate {
            name: "greet".into(),
            body: "Hello $1!".into(),
            path: PathBuf::from("/tmp/greet.md"),
            argument_hint: None,
        }],
        ..Default::default()
    };

    let harness = make_harness(script, resources).await;
    let msg = harness
        .prompt_from_template("greet", "world")
        .await
        .unwrap();
    assert_eq!(msg.role(), "assistant");
}

#[tokio::test]
async fn compact_with_aggressive_settings() {
    let script = FauxScript::new();
    let harness = make_harness(script, Default::default()).await;

    harness
        .set_compaction_settings(CompactionSettings {
            enabled: true,
            reserve_tokens: 0,
            keep_recent_tokens: 1,
        })
        .await;

    for i in 0..30 {
        harness
            .append_message(AgentMessage::user_text(format!(
                "msg {i} {}",
                "x".repeat(800)
            )))
            .await
            .unwrap();
    }

    let result = harness.compact(None).await.unwrap();
    assert!(!result.summary.is_empty());
    assert!(result.tokens_before > 0);
}

#[tokio::test]
async fn navigate_tree_moves_leaf() {
    let store = create_in_memory_session_store();
    let repo = create_session_repository(store, None);
    let session = repo.create(None, None).await.unwrap();
    let e1 = session
        .append_message(AgentMessage::user_text("one"))
        .await
        .unwrap();
    session
        .append_message(AgentMessage::user_text("two"))
        .await
        .unwrap();
    session
        .append_message(AgentMessage::user_text("three"))
        .await
        .unwrap();
    let first_id = e1.id().to_string();

    let models = Arc::new(Models::new());
    models.set_provider(faux_provider(FauxScript::new()));
    let model = models.get_model("faux", "faux-model").unwrap();
    let host = Arc::new(HostExecutionEnv::new(std::env::temp_dir()));
    let harness = AgentHarness::new(AgentHarnessOptions {
        models,
        model: Some(model),
        session,
        host_env: host,
        tools: vec![],
        system_prompt: "sys".into(),
        sandbox: SandboxMode::Disabled,
        resources: Default::default(),
    });

    let ctx_before = harness.create_turn_state().await.unwrap();
    assert_eq!(ctx_before.messages.len(), 3);

    let nav = harness.navigate_tree(&first_id, false).await.unwrap();
    assert!(!nav.cancelled);

    let ctx_after = harness.create_turn_state().await.unwrap();
    assert_eq!(ctx_after.messages.len(), 1);
}

#[tokio::test]
async fn request_shutdown_clears_and_rejects_prompt() {
    let script = FauxScript::new();
    script.push(FauxResponse::Text("nope".into()));
    let harness = make_harness(script, Default::default()).await;

    harness.steer(AgentMessage::user_text("steer"));
    harness.follow_up(AgentMessage::user_text("follow"));
    harness.next_turn(AgentMessage::user_text("next"));

    harness.request_shutdown();

    assert!(harness.is_shutting_down());
    let err = harness.prompt("hello").await.unwrap_err();
    assert!(matches!(
        err,
        loop_agent::harness::AgentHarnessError::ShuttingDown
    ));

    harness.wait_for_shutdown().await;
}

#[tokio::test]
async fn hook_cancels_compact() {
    let script = FauxScript::new();
    let harness = make_harness(script, Default::default()).await;

    harness.on(|event| async move {
        if matches!(event, HarnessHookEvent::SessionBeforeCompact { .. }) {
            HookOutcome {
                cancel: true,
                ..Default::default()
            }
        } else {
            HookOutcome::default()
        }
    });

    harness
        .set_compaction_settings(CompactionSettings {
            enabled: true,
            reserve_tokens: 0,
            keep_recent_tokens: 1,
        })
        .await;

    for i in 0..20 {
        harness
            .append_message(AgentMessage::user_text(format!("m{i} {}", "y".repeat(500))))
            .await
            .unwrap();
    }

    let err = harness.compact(None).await.unwrap_err();
    assert!(matches!(
        err,
        loop_agent::harness::AgentHarnessError::Hook(_)
    ));
}
