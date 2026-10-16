//! Declarative JSON hooks → harness HookRegistry.
//!
//! A hook file names an event (`on`), optional `match` rules for tool events,
//! and an `action`:
//!
//! ```json
//! { "on": "tool_call",
//!   "match": { "kind": "execute", "tool": "bash", "args": { "command": "*git push*--force*" } },
//!   "action": { "block": true, "reason": "No force pushes" } }
//! ```

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::Deserialize;

use loop_agent::harness::{
    HarnessHookEvent, HookOutcome, HookRegistry, ToolCallInfo, ToolDecision, ToolSource,
};

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct HookFile {
    #[serde(default)]
    name: Option<String>,
    on: String,
    #[serde(rename = "match", alias = "matchRules", default)]
    match_rules: Option<MatchRules>,
    action: HookAction,
}

/// Which tool calls a hook applies to. Every rule given must match.
#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MatchRules {
    /// Glob on the tool name; `|` separates alternatives (`bash|exec*`).
    #[serde(default)]
    tool: Option<String>,
    /// Tool kind: `read`, `edit`, `delete`, `move`, `search`, `execute`,
    /// `fetch`, `think` or `other`.
    #[serde(default)]
    kind: Option<String>,
    /// `native`, `mcp`, `mcp:<server>`, `agent` or `agent:<id>`.
    #[serde(default)]
    source: Option<String>,
    /// Argument key (or JSON pointer starting with `/`) → glob on its value.
    #[serde(default)]
    args: BTreeMap<String, String>,
    /// Deprecated: substring of the JSON-encoded arguments.
    #[serde(default)]
    args_contains: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct HookAction {
    /// Block the tool call (or cancel the operation, for other events).
    #[serde(default)]
    block: bool,
    /// Run the tool call without asking (deny rules still apply).
    #[serde(default)]
    allow: bool,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    summary: Option<String>,
    /// Text added to the tool result the model sees (`tool_result`).
    #[serde(default)]
    append_context: Option<String>,
    /// Program and arguments to run (not a shell string); see
    /// [`crate::hook_command`].
    #[serde(default)]
    command: Option<Vec<String>>,
    /// Time limit for `command`, in milliseconds.
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// What a failed `command` means: `block` (default for `tool_call`)
    /// or `ignore` (default otherwise).
    #[serde(default)]
    on_error: Option<OnError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
enum OnError {
    Block,
    Ignore,
}

/// Events a hook file can name in `on`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum On {
    BeforeAgentStart,
    SessionBeforeCompact,
    SessionBeforeTree,
    Settled,
    QueueUpdate,
    ShutdownRequested,
    ToolCall,
    ToolResult,
}

impl On {
    fn parse(name: &str) -> Option<Self> {
        Some(match name {
            "before_agent_start" | "BeforeAgentStart" => Self::BeforeAgentStart,
            "session_before_compact" | "SessionBeforeCompact" => Self::SessionBeforeCompact,
            "session_before_tree" | "SessionBeforeTree" => Self::SessionBeforeTree,
            "settled" | "Settled" => Self::Settled,
            "queue_update" | "QueueUpdate" => Self::QueueUpdate,
            "shutdown_requested" | "ShutdownRequested" => Self::ShutdownRequested,
            "tool_call" | "ToolCall" => Self::ToolCall,
            "tool_result" | "ToolResult" => Self::ToolResult,
            _ => return None,
        })
    }

    fn name(self) -> &'static str {
        match self {
            Self::BeforeAgentStart => "before_agent_start",
            Self::SessionBeforeCompact => "session_before_compact",
            Self::SessionBeforeTree => "session_before_tree",
            Self::Settled => "settled",
            Self::QueueUpdate => "queue_update",
            Self::ShutdownRequested => "shutdown_requested",
            Self::ToolCall => "tool_call",
            Self::ToolResult => "tool_result",
        }
    }

    fn is_tool_event(self) -> bool {
        matches!(self, Self::ToolCall | Self::ToolResult)
    }

    fn matches(self, event: &HarnessHookEvent) -> bool {
        matches!(
            (self, event),
            (
                Self::BeforeAgentStart,
                HarnessHookEvent::BeforeAgentStart { .. }
            ) | (
                Self::SessionBeforeCompact,
                HarnessHookEvent::SessionBeforeCompact { .. }
            ) | (
                Self::SessionBeforeTree,
                HarnessHookEvent::SessionBeforeTree { .. }
            ) | (Self::Settled, HarnessHookEvent::Settled)
                | (Self::QueueUpdate, HarnessHookEvent::QueueUpdate)
                | (Self::ShutdownRequested, HarnessHookEvent::ShutdownRequested)
                | (Self::ToolCall, HarnessHookEvent::ToolCall(_))
                | (Self::ToolResult, HarnessHookEvent::ToolResult { .. })
        )
    }
}

/// A hook file that parsed and names a known event.
#[derive(Debug, Clone)]
struct Hook {
    label: String,
    on: On,
    rules: MatchRules,
    action: HookAction,
}

impl Hook {
    async fn outcome(&self, event: &HarnessHookEvent) -> Option<HookOutcome> {
        if !self.on.matches(event) {
            return None;
        }
        if let Some(call) = tool_call_of(event) {
            if !self.rules.matches(call) {
                return None;
            }
        }
        let mut outcome = HookOutcome {
            summary: self.action.summary.clone(),
            append_context: self.action.append_context.clone(),
            ..Default::default()
        };
        let mut block = self.action.block.then(|| self.block_reason(None));
        let mut allow = self.action.allow;
        if let Some(argv) = &self.action.command {
            match self.run_command(argv, event).await {
                Ok(answer) => {
                    match answer.block {
                        Some(true) => {
                            block = Some(self.block_reason(answer.reason.as_deref()));
                        }
                        Some(false) => allow = true,
                        None => {}
                    }
                    if let Some(extra) = answer.append_context {
                        outcome.append_context = Some(match outcome.append_context.take() {
                            Some(existing) => format!("{existing}\n{extra}"),
                            None => extra,
                        });
                    }
                }
                Err(error) => {
                    let message = format!("hook {} failed: {error}", self.label);
                    if self.on_error() == OnError::Block {
                        block = Some(message);
                    } else {
                        tracing::warn!("{message}");
                    }
                }
            }
        }
        match self.on {
            On::ToolCall => {
                outcome.decision = match block {
                    Some(reason) => Some(ToolDecision::Block { reason }),
                    None if allow => Some(ToolDecision::Allow),
                    None => None,
                };
            }
            On::ToolResult => {}
            _ => {
                if let Some(reason) = block {
                    outcome.cancel = true;
                    tracing::warn!("{reason}");
                }
            }
        }
        Some(outcome)
    }

    fn on_error(&self) -> OnError {
        self.action.on_error.unwrap_or(if self.on == On::ToolCall {
            OnError::Block
        } else {
            OnError::Ignore
        })
    }

    async fn run_command(
        &self,
        argv: &[String],
        event: &HarnessHookEvent,
    ) -> Result<crate::hook_command::CommandAnswer, String> {
        let call = tool_call_of(event);
        let cwd = call
            .map(|c| c.cwd.clone())
            .filter(|p| !p.as_os_str().is_empty())
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        let env = [
            ("LOOP_HOOK_EVENT", self.on.name().to_owned()),
            (
                "LOOP_SESSION_ID",
                call.map(|c| c.session_id.clone()).unwrap_or_default(),
            ),
            ("LOOP_PROJECT_DIR", cwd.to_string_lossy().into_owned()),
        ];
        let timeout = self
            .action
            .timeout_ms
            .unwrap_or(crate::hook_command::DEFAULT_TIMEOUT_MS)
            .min(crate::hook_command::MAX_TIMEOUT_MS);
        crate::hook_command::run(
            argv,
            &event_json(self.on, event),
            &cwd,
            &env,
            std::time::Duration::from_millis(timeout),
        )
        .await
    }

    /// The reason a block reports: the command's, else the file's.
    fn block_reason(&self, from_command: Option<&str>) -> String {
        match from_command.or(self.action.reason.as_deref()) {
            Some(reason) => format!("Blocked by hook {}: {reason}", self.label),
            None => format!("Blocked by hook {}", self.label),
        }
    }
}

/// The JSON a command hook reads on stdin.
fn event_json(on: On, event: &HarnessHookEvent) -> serde_json::Value {
    let mut json = match event {
        HarnessHookEvent::ToolCall(call) => serde_json::to_value(call).unwrap_or_default(),
        HarnessHookEvent::ToolResult {
            call,
            is_error,
            output,
        } => {
            let mut json = serde_json::to_value(call).unwrap_or_default();
            json["isError"] = (*is_error).into();
            json["output"] = output.clone().into();
            json
        }
        _ => serde_json::json!({}),
    };
    json["event"] = on.name().into();
    json
}

fn tool_call_of(event: &HarnessHookEvent) -> Option<&ToolCallInfo> {
    match event {
        HarnessHookEvent::ToolCall(call) | HarnessHookEvent::ToolResult { call, .. } => Some(call),
        _ => None,
    }
}

impl MatchRules {
    fn is_empty(&self) -> bool {
        self.tool.is_none()
            && self.kind.is_none()
            && self.source.is_none()
            && self.args.is_empty()
            && self.args_contains.is_none()
    }

    fn matches(&self, call: &ToolCallInfo) -> bool {
        if let Some(tool) = &self.tool {
            if !tool
                .split('|')
                .any(|alt| glob_match(alt.trim(), &call.tool))
            {
                return false;
            }
        }
        if let Some(kind) = &self.kind {
            let actual = serde_json::to_value(call.kind)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default();
            if !kind.eq_ignore_ascii_case(&actual) {
                return false;
            }
        }
        if let Some(source) = &self.source {
            if !source_matches(source, &call.source) {
                return false;
            }
        }
        for (key, pattern) in &self.args {
            let value = if key.starts_with('/') {
                call.args.pointer(key)
            } else {
                call.args.get(key)
            };
            let text = match value {
                Some(serde_json::Value::String(s)) => s.clone(),
                Some(other) => other.to_string(),
                None => return false,
            };
            if !glob_match(pattern, &text) {
                return false;
            }
        }
        if let Some(needle) = &self.args_contains {
            if !call.args.to_string().contains(needle.as_str()) {
                return false;
            }
        }
        true
    }
}

fn source_matches(rule: &str, source: &ToolSource) -> bool {
    match (rule.split_once(':'), source) {
        (None, ToolSource::Native) => rule == "native",
        (None, ToolSource::Mcp { .. }) => rule == "mcp",
        (None, ToolSource::Agent { .. }) => rule == "agent",
        (Some(("mcp", name)), ToolSource::Mcp { server }) => glob_match(name, server),
        (Some(("agent", name)), ToolSource::Agent { id }) => glob_match(name, id),
        _ => false,
    }
}

/// `*` matches any run of characters, `?` one character; everything else is
/// literal. Case-sensitive.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// Hooks read from disk, plus what to tell the user about them.
#[derive(Debug, Default)]
pub struct LoadedHooks {
    hooks: Vec<Hook>,
    /// Problems and warnings, one line each, for the TUI or stderr.
    pub notices: Vec<String>,
}

impl LoadedHooks {
    /// Number of hooks that will run.
    pub fn len(&self) -> usize {
        self.hooks.len()
    }

    /// Whether no hook will run.
    pub fn is_empty(&self) -> bool {
        self.hooks.is_empty()
    }
}

/// Read hook files. Bad files are skipped with a notice.
pub fn load_hooks(paths: &[PathBuf]) -> LoadedHooks {
    let mut loaded = LoadedHooks::default();
    for path in paths {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(e) => {
                loaded
                    .notices
                    .push(format!("hook {}: cannot read: {e}", path.display()));
                continue;
            }
        };
        let file = match serde_json::from_str::<HookFile>(&raw) {
            Ok(file) => file,
            Err(e) => {
                loaded
                    .notices
                    .push(format!("hook {}: invalid: {e}", path.display()));
                continue;
            }
        };
        let label = file.name.clone().unwrap_or_else(|| {
            path.file_stem()
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_else(|| path.display().to_string())
        });
        let Some(on) = On::parse(&file.on) else {
            loaded.notices.push(format!(
                "hook {label}: unknown event \"{}\"; it will not run",
                file.on
            ));
            continue;
        };
        let rules = file.match_rules.unwrap_or_default();
        if !on.is_tool_event() && !rules.is_empty() {
            loaded.notices.push(format!(
                "hook {label}: `match` only applies to tool_call and tool_result; ignored"
            ));
        }
        if rules.args_contains.is_some() {
            loaded.notices.push(format!(
                "hook {label}: `argsContains` is deprecated; use `args` globs"
            ));
        }
        if on == On::ToolCall && file.action.block && rules.is_empty() {
            loaded.notices.push(format!(
                "hook {label}: blocks every tool call (tool_call with block and no `match`)"
            ));
        }
        if let Some(argv) = &file.action.command {
            if argv.is_empty() {
                loaded
                    .notices
                    .push(format!("hook {label}: `command` is empty; it will not run"));
                continue;
            }
        }
        if file
            .action
            .timeout_ms
            .is_some_and(|ms| ms > crate::hook_command::MAX_TIMEOUT_MS)
        {
            loaded.notices.push(format!(
                "hook {label}: timeoutMs is capped at {} ms",
                crate::hook_command::MAX_TIMEOUT_MS
            ));
        }
        if file.action.block && file.action.allow {
            loaded.notices.push(format!(
                "hook {label}: sets both block and allow; block wins"
            ));
        }
        loaded.hooks.push(Hook {
            label,
            on,
            rules,
            action: file.action,
        });
    }
    loaded
}

/// Register `hooks` on `registry` as one handler.
pub fn register_hooks(registry: &HookRegistry, hooks: LoadedHooks) {
    if hooks.hooks.is_empty() {
        return;
    }
    let hooks = Arc::new(hooks.hooks);
    registry.on(move |event| {
        let hooks = Arc::clone(&hooks);
        async move {
            let mut merged = HookOutcome::default();
            for hook in hooks.iter() {
                if let Some(outcome) = hook.outcome(&event).await {
                    merged.merge(outcome);
                }
            }
            merged
        }
    });
}

/// Load hook files and register them on the harness; returns the notices.
pub fn register_json_hooks(
    harness: &loop_agent::harness::AgentHarness,
    paths: &[PathBuf],
) -> Vec<String> {
    let mut loaded = load_hooks(paths);
    let notices = std::mem::take(&mut loaded.notices);
    for notice in &notices {
        tracing::warn!("{notice}");
    }
    register_hooks(&harness.hooks(), loaded);
    notices
}

/// Validate a single hook file (tests / /reload).
pub fn parse_hook_file(path: &Path) -> anyhow::Result<()> {
    let raw = std::fs::read_to_string(path)?;
    let file: HookFile = serde_json::from_str(&raw)?;
    if On::parse(&file.on).is_none() {
        anyhow::bail!("unknown event \"{}\"", file.on);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use loop_protocol::ToolKind;
    use serde_json::json;

    fn call(tool: &str, kind: ToolKind, args: serde_json::Value) -> ToolCallInfo {
        ToolCallInfo {
            source: ToolSource::of_native_tool(tool),
            tool: tool.into(),
            kind,
            args,
            call_id: "c1".into(),
            session_id: "s1".into(),
            cwd: PathBuf::from("/tmp"),
        }
    }

    fn write_hooks(files: &[(&str, serde_json::Value)]) -> (tempfile::TempDir, Vec<PathBuf>) {
        let dir = tempfile::tempdir().unwrap();
        let paths = files
            .iter()
            .map(|(name, body)| {
                let path = dir.path().join(format!("{name}.json"));
                std::fs::write(&path, body.to_string()).unwrap();
                path
            })
            .collect();
        (dir, paths)
    }

    #[test]
    fn globs() {
        assert!(glob_match(
            "*git push*--force*",
            "git push origin main --force"
        ));
        assert!(glob_match("exec*", "exec_command"));
        assert!(glob_match("b?sh", "bash"));
        assert!(!glob_match("bash", "bash2"));
        assert!(glob_match("*", ""));
        assert!(!glob_match("a*b", "acd"));
    }

    #[test]
    fn rules_match_tool_kind_source_and_args() {
        let force = call(
            "bash",
            ToolKind::Execute,
            json!({"command": "git push --force"}),
        );
        let rules = |v: serde_json::Value| serde_json::from_value::<MatchRules>(v).unwrap();
        assert!(rules(json!({"tool": "write|bash"})).matches(&force));
        assert!(!rules(json!({"tool": "write|edit"})).matches(&force));
        assert!(rules(json!({"kind": "execute"})).matches(&force));
        assert!(!rules(json!({"kind": "edit"})).matches(&force));
        assert!(rules(json!({"source": "native"})).matches(&force));
        assert!(!rules(json!({"source": "mcp"})).matches(&force));
        assert!(rules(json!({"args": {"command": "*--force*"}})).matches(&force));
        assert!(rules(json!({"args": {"/command": "git *"}})).matches(&force));
        assert!(
            !rules(json!({"args": {"cwd": "*"}})).matches(&force),
            "a missing argument does not match"
        );
        assert!(rules(json!({"argsContains": "--force"})).matches(&force));

        let mcp = call("mcp__github__create_issue", ToolKind::Other, json!({}));
        assert!(rules(json!({"source": "mcp:git*"})).matches(&mcp));
        assert!(!rules(json!({"source": "mcp:slack"})).matches(&mcp));
    }

    #[test]
    fn unknown_match_fields_are_rejected() {
        let (_dir, paths) = write_hooks(&[(
            "typo",
            json!({"on": "tool_call", "match": {"tools": "bash"}, "action": {"block": true}}),
        )]);
        let loaded = load_hooks(&paths);
        assert!(loaded.is_empty());
        assert!(
            loaded.notices[0].contains("invalid"),
            "{:?}",
            loaded.notices
        );
    }

    #[test]
    fn notices_for_risky_or_odd_files() {
        let (_dir, paths) = write_hooks(&[
            ("all", json!({"on": "tool_call", "action": {"block": true}})),
            ("unknown", json!({"on": "tool_called", "action": {}})),
            (
                "old",
                json!({"on": "tool_call", "match": {"argsContains": "x"}, "action": {}}),
            ),
        ]);
        let loaded = load_hooks(&paths);
        assert_eq!(loaded.len(), 2);
        let text = loaded.notices.join("\n");
        assert!(text.contains("all: blocks every tool call"), "{text}");
        assert!(text.contains("unknown: unknown event"), "{text}");
        assert!(text.contains("old: `argsContains` is deprecated"), "{text}");
    }

    #[tokio::test]
    async fn tool_hooks_decide_and_add_context() {
        let (_dir, paths) = write_hooks(&[
            (
                "no-force",
                json!({"on": "tool_call", "match": {"tool": "bash", "args": {"command": "*--force*"}},
                       "action": {"block": true, "reason": "no force pushes"}}),
            ),
            (
                "reads-ok",
                json!({"on": "tool_call", "match": {"kind": "read"}, "action": {"allow": true}}),
            ),
            (
                "note",
                json!({"on": "tool_result", "match": {"tool": "bash"},
                       "action": {"appendContext": "remember to run tests"}}),
            ),
        ]);
        let registry = HookRegistry::default();
        register_hooks(&registry, load_hooks(&paths));

        let force = call(
            "bash",
            ToolKind::Execute,
            json!({"command": "git push --force"}),
        );
        let outcome = registry.emit(HarnessHookEvent::ToolCall(force)).await;
        assert_eq!(
            outcome.decision,
            Some(ToolDecision::Block {
                reason: "Blocked by hook no-force: no force pushes".into()
            })
        );
        assert!(!outcome.cancel, "tool hooks decide, they do not cancel");

        let plain = call("bash", ToolKind::Execute, json!({"command": "ls"}));
        let outcome = registry
            .emit(HarnessHookEvent::ToolCall(plain.clone()))
            .await;
        assert_eq!(outcome.decision, None);

        let read = call("read", ToolKind::Read, json!({"path": "a"}));
        let outcome = registry.emit(HarnessHookEvent::ToolCall(read)).await;
        assert_eq!(outcome.decision, Some(ToolDecision::Allow));

        let outcome = registry
            .emit(HarnessHookEvent::ToolResult {
                call: plain,
                is_error: false,
                output: "ok".into(),
            })
            .await;
        assert_eq!(
            outcome.append_context.as_deref(),
            Some("remember to run tests")
        );
        assert_eq!(outcome.decision, None);
    }

    #[tokio::test]
    async fn lifecycle_hooks_still_cancel_and_summarise() {
        let (_dir, paths) = write_hooks(&[(
            "compact",
            json!({"on": "session_before_compact", "action": {"block": true, "summary": "s"}}),
        )]);
        let registry = HookRegistry::default();
        register_hooks(&registry, load_hooks(&paths));
        let outcome = registry
            .emit(HarnessHookEvent::SessionBeforeCompact {
                preparation_cut: 0,
                custom_instructions: None,
            })
            .await;
        assert!(outcome.cancel);
        assert_eq!(outcome.summary.as_deref(), Some("s"));
    }

    #[cfg(unix)]
    async fn emit_with(hook: serde_json::Value, event: HarnessHookEvent) -> HookOutcome {
        let (_dir, paths) = write_hooks(&[("cmd", hook)]);
        let loaded = load_hooks(&paths);
        assert!(loaded.notices.is_empty(), "{:?}", loaded.notices);
        let registry = HookRegistry::default();
        register_hooks(&registry, loaded);
        registry.emit(event).await
    }

    #[cfg(unix)]
    fn bash_call(command: &str) -> HarnessHookEvent {
        HarnessHookEvent::ToolCall(call(
            "bash",
            ToolKind::Execute,
            json!({ "command": command }),
        ))
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn command_hooks_decide_from_exit_code_and_stdin() {
        let script = r#"grep -q 'rm -rf' && { echo 'destructive' >&2; exit 2; }; exit 0"#;
        let hook = json!({"on": "tool_call", "action": {"command": ["sh", "-c", script]}});
        let outcome = emit_with(hook.clone(), bash_call("rm -rf build")).await;
        assert_eq!(
            outcome.decision,
            Some(ToolDecision::Block {
                reason: "Blocked by hook cmd: destructive".into()
            })
        );
        let outcome = emit_with(hook, bash_call("ls")).await;
        assert_eq!(outcome.decision, None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn failing_tool_call_hooks_block_unless_told_to_ignore() {
        let failing = json!({"on": "tool_call", "action": {"command": ["sh", "-c", "exit 7"]}});
        let outcome = emit_with(failing, bash_call("ls")).await;
        match outcome.decision {
            Some(ToolDecision::Block { reason }) => {
                assert!(
                    reason.contains("hook cmd failed: exited with code 7"),
                    "{reason}"
                )
            }
            other => panic!("expected a block, got {other:?}"),
        }

        let slow = json!({"on": "tool_call",
                          "action": {"command": ["sleep", "5"], "timeoutMs": 200}});
        let outcome = emit_with(slow, bash_call("ls")).await;
        match outcome.decision {
            Some(ToolDecision::Block { reason }) => {
                assert!(reason.contains("timed out"), "{reason}")
            }
            other => panic!("expected a block, got {other:?}"),
        }

        let ignored = json!({"on": "tool_call",
                             "action": {"command": ["sh", "-c", "exit 7"], "onError": "ignore"}});
        assert_eq!(emit_with(ignored, bash_call("ls")).await.decision, None);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tool_result_command_hooks_add_context_and_fail_open() {
        let event = HarnessHookEvent::ToolResult {
            call: call("bash", ToolKind::Execute, json!({"command": "cargo test"})),
            is_error: true,
            output: "1 failed".into(),
        };
        let script = r#"grep -q '"isError":true' && echo '{"appendContext":"tests failed; fix before continuing"}'"#;
        let hook = json!({"on": "tool_result", "action": {"command": ["sh", "-c", script]}});
        let outcome = emit_with(hook, event.clone()).await;
        assert_eq!(
            outcome.append_context.as_deref(),
            Some("tests failed; fix before continuing")
        );

        let failing = json!({"on": "tool_result", "action": {"command": ["sh", "-c", "exit 1"]}});
        let outcome = emit_with(failing, event).await;
        assert_eq!(outcome.decision, None);
        assert!(!outcome.cancel);
    }

    #[test]
    fn empty_commands_are_rejected() {
        let (_dir, paths) = write_hooks(&[(
            "empty",
            json!({"on": "tool_call", "action": {"command": []}}),
        )]);
        let loaded = load_hooks(&paths);
        assert!(loaded.is_empty());
        assert!(loaded.notices[0].contains("`command` is empty"));
    }
}
