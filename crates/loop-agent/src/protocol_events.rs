//! Translate [`AgentEvent`]s from Loop's own agent loop into
//! [`loop_protocol::SessionUpdate`]s, the event model every surface reads.
//!
//! One agent run (from `AgentStart` to `AgentEnd`) is one protocol turn. The
//! loop's internal per-model-call `TurnStart`/`TurnEnd` events are not turns
//! in the protocol sense and produce no updates.

use loop_ai::{AssistantContent, AssistantMessage, AssistantMessageEvent, Message};
use loop_ai::{StopReason as AiStopReason, ToolResultContent, UserContent, UserMessageContent};
use loop_protocol::{
    ContentBlock, ErrorCategory, ErrorInfo, MessageId, NoticeLevel, Role, SessionUpdate,
    StopReason, ToolCall, ToolCallId, ToolCallStatus, ToolCallUpdate, ToolContent, ToolKind,
    ToolLocation, TurnId, Usage,
};
use serde_json::Value;

use crate::types::{AgentEvent, AgentMessage, AgentToolResult};

/// Longest tool title shown for a shell command before it is shortened.
const MAX_TITLE_CHARS: usize = 80;

/// Stateful translator for one session's event stream.
///
/// Feed every [`AgentEvent`] in order to [`NativeEventMapper::map`]. Ids for
/// turns and messages are assigned here and are unique within the mapper.
#[derive(Debug, Default)]
pub struct NativeEventMapper {
    next_turn: u64,
    next_message: u64,
    turn: Option<TurnId>,
    message: Option<OpenMessage>,
    last_assistant: Option<AssistantMessage>,
}

#[derive(Debug)]
struct OpenMessage {
    id: MessageId,
    role: Role,
    /// Whether any text has been sent for this message as chunks.
    streamed_text: bool,
}

impl NativeEventMapper {
    /// A mapper with no turn in progress.
    pub fn new() -> Self {
        Self::default()
    }

    /// The turn in progress, if any.
    pub fn current_turn(&self) -> Option<&TurnId> {
        self.turn.as_ref()
    }

    /// How many turns have started so far.
    pub fn turns_started(&self) -> u64 {
        self.next_turn
    }

    /// Start a turn without an agent event, for a run that fails before the
    /// agent loop starts. Pair with [`NativeEventMapper::end_turn_with_error`].
    pub fn begin_turn(&mut self) -> TurnId {
        self.open_turn()
    }

    /// End the turn in progress as failed, closing any open message first.
    pub fn end_turn_with_error(&mut self, error: ErrorInfo) -> Vec<SessionUpdate> {
        let mut updates = self.close_open_message();
        if let Some(turn_id) = self.turn.take() {
            updates.push(SessionUpdate::TurnEnded {
                turn_id,
                stop: StopReason::Error,
                error: Some(error),
            });
        }
        self.last_assistant = None;
        updates
    }

    /// Updates for one agent event, in order. Many events produce none.
    pub fn map(&mut self, event: &AgentEvent) -> Vec<SessionUpdate> {
        match event {
            AgentEvent::AgentStart => self.start_turn(),
            AgentEvent::AgentEnd { messages } => self.end_turn(messages),
            AgentEvent::Progress { message } => vec![SessionUpdate::Notice {
                level: NoticeLevel::Info,
                text: message.clone(),
            }],
            AgentEvent::TurnStart | AgentEvent::TurnEnd { .. } => Vec::new(),
            AgentEvent::MessageStart { message } => self.message_start(message),
            AgentEvent::MessageUpdate {
                assistant_message_event,
                ..
            } => self.message_update(assistant_message_event),
            AgentEvent::MessageEnd { message } => self.message_end(message),
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => vec![SessionUpdate::ToolCall(tool_call(
                tool_call_id,
                tool_name,
                args,
            ))],
            AgentEvent::ToolExecutionUpdate {
                tool_call_id,
                partial_result,
                ..
            } => {
                let mut update = ToolCallUpdate::new(ToolCallId::from(tool_call_id.as_str()));
                update.content = Some(text_content(&partial_result.content));
                vec![SessionUpdate::ToolCallUpdate(update)]
            }
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                result,
                is_error,
                ..
            } => vec![SessionUpdate::ToolCallUpdate(tool_call_end(
                tool_call_id,
                result,
                *is_error,
            ))],
        }
    }

    fn start_turn(&mut self) -> Vec<SessionUpdate> {
        vec![SessionUpdate::TurnStarted {
            turn_id: self.open_turn(),
        }]
    }

    fn open_turn(&mut self) -> TurnId {
        self.next_turn += 1;
        let turn_id = TurnId(format!("turn-{}", self.next_turn));
        self.turn = Some(turn_id.clone());
        self.message = None;
        self.last_assistant = None;
        turn_id
    }

    fn end_turn(&mut self, messages: &[AgentMessage]) -> Vec<SessionUpdate> {
        let mut updates = self.close_open_message();
        let Some(turn_id) = self.turn.take() else {
            return updates;
        };
        let last = messages
            .iter()
            .rev()
            .find_map(AgentMessage::as_assistant)
            .or(self.last_assistant.as_ref());
        let (stop, error) = match last {
            Some(message) => turn_outcome(message),
            None => (StopReason::EndTurn, None),
        };
        self.last_assistant = None;
        updates.push(SessionUpdate::TurnEnded {
            turn_id,
            stop,
            error,
        });
        updates
    }

    fn message_start(&mut self, message: &AgentMessage) -> Vec<SessionUpdate> {
        let role = match message {
            AgentMessage::Llm(Message::User(_)) => Role::User,
            AgentMessage::Llm(Message::Assistant(_)) => Role::Agent,
            // Tool results are reported through tool call updates; harness
            // messages (summaries, bash records) are not conversation.
            _ => return Vec::new(),
        };
        let mut updates = self.close_open_message();
        let id = self.open_message(role);
        updates.push(SessionUpdate::MessageStarted {
            message_id: id.clone(),
            role,
        });
        if let AgentMessage::Llm(Message::User(user)) = message {
            for content in user_content(&user.content) {
                updates.push(SessionUpdate::MessageChunk {
                    message_id: id.clone(),
                    role,
                    content,
                });
            }
            if let Some(open) = self.message.as_mut() {
                open.streamed_text = true;
            }
        }
        updates
    }

    fn message_update(&mut self, event: &AssistantMessageEvent) -> Vec<SessionUpdate> {
        let (delta, thought) = match event {
            AssistantMessageEvent::TextDelta { delta, .. } => (delta, false),
            AssistantMessageEvent::ThinkingDelta { delta, .. } => (delta, true),
            _ => return Vec::new(),
        };
        if delta.is_empty() {
            return Vec::new();
        }
        let mut updates = Vec::new();
        if !matches!(&self.message, Some(open) if open.role == Role::Agent) {
            updates.extend(self.close_open_message());
            let message_id = self.open_message(Role::Agent);
            updates.push(SessionUpdate::MessageStarted {
                message_id,
                role: Role::Agent,
            });
        }
        let Some(open) = self.message.as_mut() else {
            return updates;
        };
        let message_id = open.id.clone();
        let content = ContentBlock::text(delta.clone());
        if thought {
            updates.push(SessionUpdate::ThoughtChunk {
                message_id,
                content,
            });
        } else {
            open.streamed_text = true;
            updates.push(SessionUpdate::MessageChunk {
                message_id,
                role: Role::Agent,
                content,
            });
        }
        updates
    }

    fn message_end(&mut self, message: &AgentMessage) -> Vec<SessionUpdate> {
        let AgentMessage::Llm(llm) = message else {
            return Vec::new();
        };
        let assistant = match llm {
            Message::Assistant(assistant) => assistant,
            Message::User(_) => return self.close_open_message(),
            Message::ToolResult(_) => return Vec::new(),
        };
        let mut updates = Vec::new();
        if let Some(open) = self.message.as_ref() {
            // A message that failed before streaming arrives whole: send its
            // text now so readers that only follow chunks still see it.
            if !open.streamed_text {
                let id = open.id.clone();
                for block in &assistant.content {
                    if let AssistantContent::Text(text) = block {
                        if !text.text.is_empty() {
                            updates.push(SessionUpdate::MessageChunk {
                                message_id: id.clone(),
                                role: Role::Agent,
                                content: ContentBlock::text(text.text.clone()),
                            });
                        }
                    }
                }
            }
        }
        updates.extend(self.close_open_message());
        if let Some(usage) = usage(assistant) {
            updates.push(SessionUpdate::Usage(usage));
        }
        self.last_assistant = Some(assistant.clone());
        updates
    }

    fn open_message(&mut self, role: Role) -> MessageId {
        self.next_message += 1;
        let id = MessageId(format!("msg-{}", self.next_message));
        self.message = Some(OpenMessage {
            id: id.clone(),
            role,
            streamed_text: false,
        });
        id
    }

    fn close_open_message(&mut self) -> Vec<SessionUpdate> {
        self.message
            .take()
            .map(|open| SessionUpdate::MessageEnded {
                message_id: open.id,
            })
            .into_iter()
            .collect()
    }
}

/// Updates that rebuild a stored transcript, to be sent as
/// [`loop_protocol::Origin::Replay`].
///
/// Each user message becomes one text chunk. An assistant failure becomes an
/// error notice; summaries and other harness messages become info notices.
pub fn replay(messages: &[AgentMessage]) -> Vec<SessionUpdate> {
    use crate::types::CustomAgentMessage;

    let mut updates = Vec::new();
    let mut next_id = 0u64;
    let mut message_id = || {
        next_id += 1;
        MessageId(format!("replay-{next_id}"))
    };
    for (index, message) in messages.iter().enumerate() {
        match message {
            AgentMessage::Llm(Message::User(user)) => {
                let text: String = user_content(&user.content)
                    .iter()
                    .filter_map(ContentBlock::as_text)
                    .collect();
                if text.is_empty() {
                    continue;
                }
                let id = message_id();
                updates.push(SessionUpdate::MessageStarted {
                    message_id: id.clone(),
                    role: Role::User,
                });
                updates.push(SessionUpdate::MessageChunk {
                    message_id: id.clone(),
                    role: Role::User,
                    content: ContentBlock::text(text),
                });
                updates.push(SessionUpdate::MessageEnded { message_id: id });
            }
            AgentMessage::Llm(Message::Assistant(assistant)) => {
                let id = message_id();
                updates.push(SessionUpdate::MessageStarted {
                    message_id: id.clone(),
                    role: Role::Agent,
                });
                for block in &assistant.content {
                    match block {
                        AssistantContent::Thinking(t) if !t.thinking.is_empty() => {
                            updates.push(SessionUpdate::ThoughtChunk {
                                message_id: id.clone(),
                                content: ContentBlock::text(t.thinking.clone()),
                            });
                        }
                        AssistantContent::Text(t) if !t.text.is_empty() => {
                            updates.push(SessionUpdate::MessageChunk {
                                message_id: id.clone(),
                                role: Role::Agent,
                                content: ContentBlock::text(t.text.clone()),
                            });
                        }
                        AssistantContent::ToolCall(call) => {
                            let mut call = tool_call(&call.id, &call.name, &call.arguments);
                            call.status = ToolCallStatus::Pending;
                            updates.push(SessionUpdate::ToolCall(call));
                        }
                        _ => {}
                    }
                }
                updates.push(SessionUpdate::MessageEnded { message_id: id });
                if assistant.stop_reason == AiStopReason::Error {
                    if let Some(error) = assistant.failure() {
                        updates.push(SessionUpdate::Notice {
                            level: NoticeLevel::Error,
                            text: error,
                        });
                    }
                }
            }
            AgentMessage::Llm(Message::ToolResult(result)) => {
                let mut update =
                    ToolCallUpdate::new(ToolCallId::from(result.tool_call_id.as_str()));
                update.name = Some(result.tool_name.clone());
                update.status = Some(if result.is_error {
                    ToolCallStatus::Failed
                } else {
                    ToolCallStatus::Completed
                });
                update.content = Some(text_content(&result.content));
                updates.push(SessionUpdate::ToolCallUpdate(update));
            }
            AgentMessage::Custom(custom) => updates.push(match custom {
                CustomAgentMessage::CompactionSummary { summary, .. } => {
                    info(format!("compaction summary\n{summary}"))
                }
                CustomAgentMessage::BranchSummary { summary, .. } => {
                    info(format!("branch summary\n{summary}"))
                }
                CustomAgentMessage::Custom {
                    custom_type,
                    content,
                    ..
                } => info(format!("{custom_type}: {content}")),
                CustomAgentMessage::BashExecution {
                    command,
                    output,
                    exit_code,
                    ..
                } => SessionUpdate::ToolCall(ToolCall {
                    id: ToolCallId(format!("bash-exec-{index}")),
                    name: Some("bash".into()),
                    title: shorten_command(command),
                    kind: ToolKind::Execute,
                    status: match exit_code {
                        Some(0) | None => ToolCallStatus::Completed,
                        Some(_) => ToolCallStatus::Failed,
                    },
                    content: vec![ToolContent::text(format!("$ {command}\n{output}"))],
                    locations: Vec::new(),
                    raw_input: Some(serde_json::json!({ "command": command })),
                }),
            }),
        }
    }
    updates
}

fn info(text: String) -> SessionUpdate {
    SessionUpdate::Notice {
        level: NoticeLevel::Info,
        text,
    }
}

/// The protocol kind for one of Loop's tools.
pub fn tool_kind(tool_name: &str) -> ToolKind {
    match tool_name {
        "read" => ToolKind::Read,
        "write" | "edit" => ToolKind::Edit,
        "bash" => ToolKind::Execute,
        "grep" | "find" | "ls" | "glob" => ToolKind::Search,
        "fetch" | "web_fetch" | "web_search" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

fn tool_call(id: &str, name: &str, args: &Value) -> ToolCall {
    let path = args.get("path").and_then(Value::as_str);
    let title = match (name, path) {
        ("read" | "write" | "edit", Some(path)) => format!("{name} {path}"),
        ("bash", _) => args
            .get("command")
            .and_then(Value::as_str)
            .map(shorten_command)
            .unwrap_or_else(|| name.to_owned()),
        _ => name.to_owned(),
    };
    ToolCall {
        id: ToolCallId::from(id),
        name: Some(name.to_owned()),
        title,
        kind: tool_kind(name),
        status: ToolCallStatus::InProgress,
        content: Vec::new(),
        locations: path
            .map(|path| {
                vec![ToolLocation {
                    path: path.to_owned(),
                    line: None,
                }]
            })
            .unwrap_or_default(),
        raw_input: Some(args.clone()),
    }
}

fn tool_call_end(id: &str, result: &AgentToolResult, is_error: bool) -> ToolCallUpdate {
    let mut content = Vec::new();
    let path = result.details.get("path").and_then(Value::as_str);
    let diff = result.details.get("diff").and_then(Value::as_str);
    if let (Some(path), Some(diff)) = (path, diff) {
        content.push(ToolContent::UnifiedDiff {
            path: path.to_owned(),
            diff: diff.to_owned(),
        });
    }
    content.extend(text_content(&result.content));

    let mut update = ToolCallUpdate::new(ToolCallId::from(id));
    update.status = Some(if is_error {
        ToolCallStatus::Failed
    } else {
        ToolCallStatus::Completed
    });
    update.content = Some(content);
    if result.details.as_object().is_some_and(|d| !d.is_empty()) {
        update.raw_output = Some(result.details.clone());
    }
    update
}

fn text_content(content: &[ToolResultContent]) -> Vec<ToolContent> {
    content
        .iter()
        .map(|block| match block {
            ToolResultContent::Text(text) => ToolContent::text(text.text.clone()),
            ToolResultContent::Image(image) => ToolContent::Content {
                content: ContentBlock::Image {
                    data: image.data.clone(),
                    mime_type: image.mime_type.clone(),
                },
            },
        })
        .collect()
}

fn user_content(content: &UserMessageContent) -> Vec<ContentBlock> {
    match content {
        UserMessageContent::Text(text) => vec![ContentBlock::text(text.clone())],
        UserMessageContent::Blocks(blocks) => blocks
            .iter()
            .map(|block| match block {
                UserContent::Text(text) => ContentBlock::text(text.text.clone()),
                UserContent::Image(image) => ContentBlock::Image {
                    data: image.data.clone(),
                    mime_type: image.mime_type.clone(),
                },
            })
            .collect(),
    }
}

fn usage(message: &AssistantMessage) -> Option<Usage> {
    let u = &message.usage;
    if u.total_tokens == 0 && u.input == 0 && u.output == 0 {
        return None;
    }
    Some(Usage {
        input_tokens: u.input,
        output_tokens: u.output,
        cached_input_tokens: (u.cache_read > 0).then_some(u.cache_read),
        cache_write_tokens: (u.cache_write > 0).then_some(u.cache_write),
        reasoning_tokens: u.reasoning,
        total_tokens: u.total_tokens,
        context_window: None,
        cost_usd: (u.cost.total > 0.0).then_some(u.cost.total),
    })
}

/// Why a run ended, from its final assistant message.
pub fn turn_outcome(message: &AssistantMessage) -> (StopReason, Option<ErrorInfo>) {
    match message.stop_reason {
        AiStopReason::Stop | AiStopReason::ToolUse | AiStopReason::Pending => {
            (StopReason::EndTurn, None)
        }
        AiStopReason::Length => (StopReason::MaxTokens, None),
        AiStopReason::Aborted => (StopReason::Cancelled, None),
        AiStopReason::Error => {
            let text = message
                .failure()
                .unwrap_or_else(|| "the model request failed".to_owned());
            (StopReason::Error, Some(classify_error(&text)))
        }
    }
}

/// Best-effort category for a provider failure reported as text.
fn classify_error(message: &str) -> ErrorInfo {
    let lower = message.to_ascii_lowercase();
    let has = |needle: &str| lower.contains(needle);
    let category = if has("http 401") || has("http 403") || has("unauthorized") || has("api key") {
        ErrorCategory::Auth
    } else if has("insufficient_quota") || has("quota") || has("billing") {
        ErrorCategory::Quota
    } else if has("http 429") || has("rate limit") || has("too many requests") {
        ErrorCategory::RateLimit
    } else if has("http 5")
        || has("timed out")
        || has("timeout")
        || has("connection")
        || has("network")
    {
        ErrorCategory::Network
    } else {
        ErrorCategory::Agent
    };
    ErrorInfo::new(category, message)
}

fn shorten_command(command: &str) -> String {
    let first_line = command.lines().next().unwrap_or_default();
    let mut title: String = first_line.chars().take(MAX_TITLE_CHARS).collect();
    if first_line.chars().count() > MAX_TITLE_CHARS || command.lines().nth(1).is_some() {
        title.push('…');
    }
    title
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn errors_are_classified() {
        let cat = |m: &str| classify_error(m).category;
        assert_eq!(cat("HTTP 401 Unauthorized: bad key"), ErrorCategory::Auth);
        assert_eq!(cat("HTTP 429 Too Many Requests"), ErrorCategory::RateLimit);
        assert_eq!(
            cat("insufficient_quota: plan exhausted"),
            ErrorCategory::Quota
        );
        assert_eq!(cat("HTTP 502 Bad Gateway"), ErrorCategory::Network);
        assert_eq!(cat("model refused tool schema"), ErrorCategory::Agent);
        assert!(classify_error("HTTP 429").retryable);
    }

    #[test]
    fn long_commands_are_shortened() {
        assert_eq!(shorten_command("ls -la"), "ls -la");
        assert_eq!(shorten_command("echo a\necho b"), "echo a…");
        let long = "x".repeat(MAX_TITLE_CHARS + 5);
        assert_eq!(shorten_command(&long).chars().count(), MAX_TITLE_CHARS + 1);
    }

    #[test]
    fn tool_kinds() {
        assert_eq!(tool_kind("edit"), ToolKind::Edit);
        assert_eq!(tool_kind("bash"), ToolKind::Execute);
        assert_eq!(tool_kind("mcp__github__create_issue"), ToolKind::Other);
    }
}
