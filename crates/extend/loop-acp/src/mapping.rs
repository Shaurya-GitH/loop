//! Translation between ACP v1 types and Loop's protocol.
//!
//! ACP has no message boundaries, so [`MessageTracker`] synthesizes
//! `MessageStarted`/`MessageEnded` around runs of chunks from one author.

use agent_client_protocol_schema::v1 as acp;
use loop_protocol::{
    ContentBlock, ErrorCategory, ErrorInfo, MessageId, NoticeLevel, PermissionKind,
    PermissionOption, PlanEntry, PlanEntryStatus, Role, SessionUpdate, StopReason, ToolCall,
    ToolCallId, ToolCallStatus, ToolCallUpdate, ToolContent, ToolKind, ToolLocation, Usage,
};
use serde_json::Value;

/// Opens and closes protocol messages around ACP chunks.
#[derive(Debug, Default)]
pub struct MessageTracker {
    next: u64,
    open: Option<OpenMessage>,
}

#[derive(Debug)]
struct OpenMessage {
    id: MessageId,
    role: Role,
    native: Option<String>,
}

impl MessageTracker {
    /// Close the open message, if any.
    pub fn close(&mut self) -> Option<SessionUpdate> {
        self.open.take().map(|open| SessionUpdate::MessageEnded {
            message_id: open.id,
        })
    }

    /// The message a chunk from `role` with ACP id `native` belongs to,
    /// opening (and closing the previous) one when the author or id changes.
    fn message_for(
        &mut self,
        role: Role,
        native: Option<&str>,
        out: &mut Vec<SessionUpdate>,
    ) -> MessageId {
        let same = matches!(&self.open, Some(open)
            if open.role == role && (native.is_none() || open.native.as_deref() == native));
        if !same {
            out.extend(self.close());
            self.next += 1;
            let id = MessageId(format!("acp-msg-{}", self.next));
            out.push(SessionUpdate::MessageStarted {
                message_id: id.clone(),
                role,
            });
            self.open = Some(OpenMessage {
                id,
                role,
                native: native.map(str::to_owned),
            });
        }
        self.open
            .as_ref()
            .map(|open| open.id.clone())
            .unwrap_or_else(|| MessageId("acp-msg-0".into()))
    }
}

/// Protocol updates for one ACP `session/update` payload (the `update`
/// field). Unknown update kinds come back as [`SessionUpdate::Unknown`].
pub fn session_update(raw: &Value, messages: &mut MessageTracker) -> Vec<SessionUpdate> {
    let Ok(update) = serde_json::from_value::<acp::SessionUpdate>(raw.clone()) else {
        return vec![SessionUpdate::Unknown(raw.clone())];
    };
    let mut out = Vec::new();
    match update {
        acp::SessionUpdate::UserMessageChunk(chunk) => {
            chunk_update(chunk, Role::User, false, messages, &mut out)
        }
        acp::SessionUpdate::AgentMessageChunk(chunk) => {
            chunk_update(chunk, Role::Agent, false, messages, &mut out)
        }
        acp::SessionUpdate::AgentThoughtChunk(chunk) => {
            chunk_update(chunk, Role::Agent, true, messages, &mut out)
        }
        acp::SessionUpdate::ToolCall(call) => {
            out.extend(messages.close());
            out.push(SessionUpdate::ToolCall(tool_call(call)));
        }
        acp::SessionUpdate::ToolCallUpdate(update) => {
            out.push(SessionUpdate::ToolCallUpdate(tool_call_update(update)));
        }
        acp::SessionUpdate::Plan(plan) => out.push(SessionUpdate::Plan {
            entries: plan.entries.into_iter().map(plan_entry).collect(),
        }),
        acp::SessionUpdate::UsageUpdate(usage) => out.push(SessionUpdate::Usage(Usage {
            total_tokens: usage.used,
            context_window: Some(usage.size),
            cost_usd: usage
                .cost
                .filter(|cost| cost.currency.eq_ignore_ascii_case("usd"))
                .map(|cost| cost.amount),
            ..Usage::default()
        })),
        acp::SessionUpdate::Notice(notice) => {
            let level = match notice.severity {
                acp::NoticeSeverity::Info => NoticeLevel::Info,
                acp::NoticeSeverity::Warning => NoticeLevel::Warning,
                acp::NoticeSeverity::Error => NoticeLevel::Error,
                _ => NoticeLevel::Other,
            };
            let text = match notice.description {
                Some(description) if !description.is_empty() => {
                    format!("{}: {description}", notice.title)
                }
                _ => notice.title,
            };
            out.push(SessionUpdate::Notice { level, text });
        }
        acp::SessionUpdate::CompactionUpdate(_) => out.push(SessionUpdate::Notice {
            level: NoticeLevel::Info,
            text: "the agent compacted its context".into(),
        }),
        // Known updates Loop does not show yet (modes, commands, config,
        // session titles, summary text).
        _ => {}
    }
    out
}

fn chunk_update(
    chunk: acp::ContentChunk,
    role: Role,
    thought: bool,
    messages: &mut MessageTracker,
    out: &mut Vec<SessionUpdate>,
) {
    let native = chunk.message_id.as_ref().map(|id| id.0.to_string());
    let message_id = messages.message_for(role, native.as_deref(), out);
    let content = content_block(chunk.content);
    out.push(if thought {
        SessionUpdate::ThoughtChunk {
            message_id,
            content,
        }
    } else {
        SessionUpdate::MessageChunk {
            message_id,
            role,
            content,
        }
    });
}

/// A protocol content block for an ACP one.
pub fn content_block(block: acp::ContentBlock) -> ContentBlock {
    match block {
        acp::ContentBlock::Text(text) => ContentBlock::Text { text: text.text },
        acp::ContentBlock::Image(image) => ContentBlock::Image {
            data: image.data,
            mime_type: image.mime_type,
        },
        acp::ContentBlock::ResourceLink(link) => ContentBlock::ResourceLink {
            uri: link.uri,
            name: Some(link.name),
        },
        other => ContentBlock::Unknown(serde_json::to_value(&other).unwrap_or(Value::Null)),
    }
}

/// An ACP content block for a protocol one, for prompts.
pub fn acp_content(block: &ContentBlock) -> Option<acp::ContentBlock> {
    match block {
        ContentBlock::Text { text } => {
            Some(acp::ContentBlock::Text(acp::TextContent::new(text.clone())))
        }
        ContentBlock::Image { data, mime_type } => serde_json::from_value(serde_json::json!({
            "type": "image", "data": data, "mimeType": mime_type
        }))
        .ok(),
        ContentBlock::ResourceLink { uri, name } => serde_json::from_value(serde_json::json!({
            "type": "resource_link", "uri": uri, "name": name.clone().unwrap_or_else(|| uri.clone())
        }))
        .ok(),
        ContentBlock::Unknown(_) => None,
    }
}

fn tool_call(call: acp::ToolCall) -> ToolCall {
    ToolCall {
        id: ToolCallId(call.tool_call_id.0.to_string()),
        name: call.name,
        title: call.title,
        kind: tool_kind(call.kind),
        status: tool_status(call.status),
        content: call.content.into_iter().map(tool_content).collect(),
        locations: call.locations.into_iter().map(location).collect(),
        raw_input: call.raw_input,
    }
}

/// A protocol tool call update for an ACP one.
pub fn tool_call_update(update: acp::ToolCallUpdate) -> ToolCallUpdate {
    let fields = update.fields;
    let mut out = ToolCallUpdate::new(ToolCallId(update.tool_call_id.0.to_string()));
    out.name = fields.name;
    out.title = fields.title;
    out.kind = fields.kind.map(tool_kind);
    out.status = fields.status.map(tool_status);
    out.content = fields
        .content
        .map(|content| content.into_iter().map(tool_content).collect());
    out.locations = fields
        .locations
        .map(|locations| locations.into_iter().map(location).collect());
    out.raw_output = fields.raw_output;
    out
}

fn tool_kind(kind: acp::ToolKind) -> ToolKind {
    match kind {
        acp::ToolKind::Read => ToolKind::Read,
        acp::ToolKind::Edit => ToolKind::Edit,
        acp::ToolKind::Delete => ToolKind::Delete,
        acp::ToolKind::Move => ToolKind::Move,
        acp::ToolKind::Search => ToolKind::Search,
        acp::ToolKind::Execute => ToolKind::Execute,
        acp::ToolKind::Think => ToolKind::Think,
        acp::ToolKind::Fetch => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

fn tool_status(status: acp::ToolCallStatus) -> ToolCallStatus {
    match status {
        acp::ToolCallStatus::InProgress => ToolCallStatus::InProgress,
        acp::ToolCallStatus::Completed => ToolCallStatus::Completed,
        acp::ToolCallStatus::Failed => ToolCallStatus::Failed,
        _ => ToolCallStatus::Pending,
    }
}

fn tool_content(content: acp::ToolCallContent) -> ToolContent {
    match content {
        acp::ToolCallContent::Content(content) => ToolContent::Content {
            content: content_block(content.content),
        },
        acp::ToolCallContent::Diff(diff) => ToolContent::Diff {
            path: diff.path.to_string_lossy().into_owned(),
            old_text: diff.old_text,
            new_text: diff.new_text,
        },
        acp::ToolCallContent::Terminal(terminal) => ToolContent::Terminal {
            terminal_id: terminal.terminal_id.0.to_string(),
        },
        other => ToolContent::Unknown(serde_json::to_value(&other).unwrap_or(Value::Null)),
    }
}

fn location(location: acp::ToolCallLocation) -> ToolLocation {
    ToolLocation {
        path: location.path.to_string_lossy().into_owned(),
        line: location.line,
    }
}

fn plan_entry(entry: acp::PlanEntry) -> PlanEntry {
    PlanEntry {
        content: entry.content,
        status: match entry.status {
            acp::PlanEntryStatus::Pending => PlanEntryStatus::Pending,
            acp::PlanEntryStatus::InProgress => PlanEntryStatus::InProgress,
            acp::PlanEntryStatus::Completed => PlanEntryStatus::Completed,
            _ => PlanEntryStatus::Other,
        },
    }
}

/// Protocol permission options for ACP ones, keeping the native ids.
pub fn permission_options(options: Vec<acp::PermissionOption>) -> Vec<PermissionOption> {
    options
        .into_iter()
        .map(|option| PermissionOption {
            native_id: option.option_id.0.to_string(),
            label: option.name,
            kind_hint: match option.kind {
                acp::PermissionOptionKind::AllowOnce => PermissionKind::AllowOnce,
                acp::PermissionOptionKind::AllowAlways => PermissionKind::AllowAlways,
                acp::PermissionOptionKind::RejectOnce => PermissionKind::RejectOnce,
                acp::PermissionOptionKind::RejectAlways => PermissionKind::RejectAlways,
                _ => PermissionKind::Other,
            },
        })
        .collect()
}

/// The protocol stop reason for an ACP one.
pub fn stop_reason(reason: acp::StopReason) -> StopReason {
    match reason {
        acp::StopReason::EndTurn => StopReason::EndTurn,
        acp::StopReason::MaxTokens => StopReason::MaxTokens,
        acp::StopReason::MaxTurnRequests => StopReason::MaxTurnRequests,
        acp::StopReason::Refusal => StopReason::Refusal,
        acp::StopReason::Cancelled => StopReason::Cancelled,
        _ => StopReason::Other,
    }
}

/// A protocol error for a JSON-RPC error from the agent.
pub fn remote_error(code: i64, message: &str) -> ErrorInfo {
    // -32000: authentication required.
    if code == -32000 {
        return ErrorInfo::new(ErrorCategory::Auth, message);
    }
    // -32800: request cancelled.
    if code == -32800 {
        return ErrorInfo::new(ErrorCategory::Cancelled, message);
    }
    ErrorInfo::from_message(message)
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn chunks_get_message_boundaries() {
        let mut messages = MessageTracker::default();
        let mut all = Vec::new();
        for raw in [
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Hel"}}),
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "lo"}}),
            json!({"sessionUpdate": "tool_call", "toolCallId": "t1", "title": "Run ls", "kind": "execute"}),
            json!({"sessionUpdate": "agent_message_chunk", "content": {"type": "text", "text": "Done"}}),
        ] {
            all.extend(session_update(&raw, &mut messages));
        }
        all.extend(messages.close());
        let kinds: Vec<_> = all
            .iter()
            .map(|u| {
                serde_json::to_value(u).unwrap()["type"]
                    .as_str()
                    .unwrap()
                    .to_owned()
            })
            .collect();
        assert_eq!(
            kinds,
            [
                "message_started",
                "message_chunk",
                "message_chunk",
                "message_ended",
                "tool_call",
                "message_started",
                "message_chunk",
                "message_ended"
            ]
        );
    }

    #[test]
    fn tool_call_with_diff_maps_fields() {
        let mut messages = MessageTracker::default();
        let updates = session_update(
            &json!({
                "sessionUpdate": "tool_call",
                "toolCallId": "t1",
                "title": "Edit a.txt",
                "kind": "edit",
                "status": "pending",
                "content": [{"type": "diff", "path": "/w/a.txt", "oldText": "a", "newText": "b"}],
                "locations": [{"path": "/w/a.txt", "line": 3}]
            }),
            &mut messages,
        );
        let SessionUpdate::ToolCall(call) = &updates[0] else {
            panic!("{updates:?}");
        };
        assert_eq!(call.kind, ToolKind::Edit);
        assert_eq!(call.status, ToolCallStatus::Pending);
        assert_eq!(call.locations[0].line, Some(3));
        assert!(matches!(&call.content[0], ToolContent::Diff { new_text, .. } if new_text == "b"));
    }

    #[test]
    fn unknown_update_kind_is_kept() {
        let raw = json!({"sessionUpdate": "hologram", "x": 1});
        let updates = session_update(&raw, &mut MessageTracker::default());
        assert_eq!(updates, vec![SessionUpdate::Unknown(raw)]);
    }

    #[test]
    fn auth_and_rate_limit_errors_are_categorized() {
        assert_eq!(remote_error(-32000, "log in").category, ErrorCategory::Auth);
        assert_eq!(
            remote_error(-32603, "HTTP 429 Too Many Requests").category,
            ErrorCategory::RateLimit
        );
    }
}
