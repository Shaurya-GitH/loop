//! Wire-format tests. The JSON here is the contract with readers of the
//! stream (surfaces, `--print` consumers), so a failing assertion means a
//! breaking change unless `PROTOCOL_VERSION` is bumped.

use loop_protocol::{
    ContentBlock, ErrorCategory, ErrorInfo, InteractionOutcome, InteractionRequest, PermissionKind,
    PermissionOption, Role, SessionUpdate, StopReason, ToolCall, ToolCallStatus, ToolCallUpdate,
    ToolContent, ToolKind, ToolLocation, UpdateEnvelope, PROTOCOL_VERSION,
};
use serde_json::{json, Value};

fn roundtrip(update: &SessionUpdate, expected: Value) {
    let actual = serde_json::to_value(update).unwrap();
    assert_eq!(actual, expected);
    let back: SessionUpdate = serde_json::from_value(expected).unwrap();
    assert_eq!(&back, update);
}

#[test]
fn envelope_carries_version_and_routing() {
    let env = UpdateEnvelope::live(
        "codex".into(),
        2,
        "s1".into(),
        Some("t1".into()),
        7,
        SessionUpdate::TurnStarted {
            turn_id: "t1".into(),
        },
    );
    assert_eq!(
        serde_json::to_value(&env).unwrap(),
        json!({
            "v": PROTOCOL_VERSION,
            "instance": "codex",
            "generation": 2,
            "session": "s1",
            "turn": "t1",
            "seq": 7,
            "origin": "live",
            "update": {"type": "turn_started", "turn_id": "t1"}
        })
    );
}

#[test]
fn message_chunk() {
    roundtrip(
        &SessionUpdate::MessageChunk {
            message_id: "m1".into(),
            role: Role::Agent,
            content: ContentBlock::text("hi"),
        },
        json!({
            "type": "message_chunk",
            "message_id": "m1",
            "role": "agent",
            "content": {"type": "text", "text": "hi"}
        }),
    );
}

#[test]
fn tool_call_and_update() {
    roundtrip(
        &SessionUpdate::ToolCall(ToolCall {
            id: "c1".into(),
            name: None,
            title: "edit src/main.rs".into(),
            kind: ToolKind::Edit,
            status: ToolCallStatus::InProgress,
            content: vec![],
            locations: vec![ToolLocation {
                path: "src/main.rs".into(),
                line: None,
            }],
            raw_input: Some(json!({"path": "src/main.rs"})),
        }),
        json!({
            "type": "tool_call",
            "id": "c1",
            "title": "edit src/main.rs",
            "kind": "edit",
            "status": "in_progress",
            "locations": [{"path": "src/main.rs"}],
            "raw_input": {"path": "src/main.rs"}
        }),
    );

    let mut update = ToolCallUpdate::new("c1".into());
    update.status = Some(ToolCallStatus::Completed);
    update.content = Some(vec![
        ToolContent::UnifiedDiff {
            path: "src/main.rs".into(),
            diff: "@@ -1 +1 @@\n-a\n+b\n".into(),
        },
        ToolContent::text("Edited src/main.rs"),
    ]);
    roundtrip(
        &SessionUpdate::ToolCallUpdate(update),
        json!({
            "type": "tool_call_update",
            "id": "c1",
            "status": "completed",
            "content": [
                {"type": "unified_diff", "path": "src/main.rs", "diff": "@@ -1 +1 @@\n-a\n+b\n"},
                {"type": "content", "content": {"type": "text", "text": "Edited src/main.rs"}}
            ]
        }),
    );
}

#[test]
fn permission_keeps_native_ids() {
    roundtrip(
        &SessionUpdate::InteractionRequested {
            request: InteractionRequest::Permission {
                id: "p1".into(),
                tool_call: None,
                options: vec![PermissionOption {
                    native_id: "approved-for-session".into(),
                    label: "Always".into(),
                    kind_hint: PermissionKind::AllowAlways,
                }],
            },
        },
        json!({
            "type": "interaction_requested",
            "request": {
                "kind": "permission",
                "id": "p1",
                "options": [{"native_id": "approved-for-session", "label": "Always", "kind_hint": "allow_always"}]
            }
        }),
    );
    roundtrip(
        &SessionUpdate::InteractionResolved {
            id: "p1".into(),
            outcome: InteractionOutcome::Selected {
                native_id: "approved-for-session".into(),
                note: None,
            },
        },
        json!({
            "type": "interaction_resolved",
            "id": "p1",
            "outcome": {"outcome": "selected", "native_id": "approved-for-session"}
        }),
    );
}

#[test]
fn failed_turn() {
    roundtrip(
        &SessionUpdate::TurnEnded {
            turn_id: "t1".into(),
            stop: StopReason::Error,
            error: Some(ErrorInfo::new(ErrorCategory::RateLimit, "HTTP 429")),
        },
        json!({
            "type": "turn_ended",
            "turn_id": "t1",
            "stop": "error",
            "error": {"category": "rate_limit", "retryable": true, "message": "HTTP 429"}
        }),
    );
}

#[test]
fn unknown_update_type_is_kept() {
    let raw = json!({"type": "agent_mood", "mood": "curious"});
    let update: SessionUpdate = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(update, SessionUpdate::Unknown(raw.clone()));
    assert_eq!(serde_json::to_value(&update).unwrap(), raw);
}

#[test]
fn unknown_content_and_tool_content_are_kept() {
    let raw = json!({"type": "audio", "data": "AAA"});
    let block: ContentBlock = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(block, ContentBlock::Unknown(raw.clone()));

    let raw = json!({"type": "hologram", "id": 1});
    let content: ToolContent = serde_json::from_value(raw.clone()).unwrap();
    assert_eq!(content, ToolContent::Unknown(raw));
}

#[test]
fn unknown_enum_values_become_other() {
    let call: ToolCall = serde_json::from_value(json!({
        "id": "c1", "title": "t", "kind": "teleport", "status": "pending"
    }))
    .unwrap();
    assert_eq!(call.kind, ToolKind::Other);

    let option: PermissionOption = serde_json::from_value(json!({
        "native_id": "x", "label": "x", "kind_hint": "allow_for_a_while"
    }))
    .unwrap();
    assert_eq!(option.kind_hint, PermissionKind::Other);

    let stop: StopReason = serde_json::from_value(json!("timed_out")).unwrap();
    assert_eq!(stop, StopReason::Other);
}

#[test]
fn retryable_follows_category() {
    assert!(ErrorInfo::new(ErrorCategory::Network, "x").retryable);
    assert!(!ErrorInfo::new(ErrorCategory::Quota, "x").retryable);
    assert!(!ErrorInfo::new(ErrorCategory::Auth, "x").retryable);
}
