//! Interactive tool approval (file edits + bash) with session auto-approve.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use loop_agent::harness::ExecutionEnv;
use loop_agent::{
    AfterToolCallContext, AfterToolCallResult, BeforeToolCallContext, BeforeToolCallResult,
};
use loop_ai::{TextContent, ToolResultContent};
use loop_protocol::HostServices;
use parking_lot::Mutex as SyncMutex;
use serde_json::json;
use tokio::sync::{mpsc, oneshot, Mutex};
use tokio_util::sync::CancellationToken;

use crate::interaction::{native_decision, native_permission_request};

/// Label prefix persisted on the session tree for auto-approve.
pub const AUTO_APPROVE_LABEL_PREFIX: &str = "loop.toolApproval:auto:";

/// File-edit tools share one session auto-approve bucket.
pub const GROUP_FILE: &str = "file";
/// Bash tool group.
pub const GROUP_BASH: &str = "bash";
/// Web tool group (`web_fetch` + `web_search`).
pub const GROUP_WEB: &str = "web";

/// Policy for when `ask`-mode tools prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalPolicy {
    /// Prompt only on brand-new sessions (not `--resume`).
    NewSession,
    /// Always prompt (unless session auto-approve).
    Always,
    /// Never prompt.
    Never,
}

impl ApprovalPolicy {
    /// Parse settings value.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "always" | "on" | "true" => Self::Always,
            "never" | "off" | "false" => Self::Never,
            _ => Self::NewSession,
        }
    }

    /// Settings / status label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NewSession => "newSession",
            Self::Always => "always",
            Self::Never => "never",
        }
    }

    /// Whether asking is active for this session (before auto-approve).
    pub fn asks_for_session(self, is_new_session: bool) -> bool {
        match self {
            Self::Always => true,
            Self::Never => false,
            Self::NewSession => is_new_session,
        }
    }
}

/// Per-tool default from `~/.loop/agent/settings.json`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPermission {
    /// Prompt when approval policy says so.
    Ask,
    /// Never prompt.
    Allow,
    /// Always block.
    Deny,
}

impl ToolPermission {
    /// Parse `ask` / `allow` / `deny`.
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "allow" | "auto" | "yes" => Self::Allow,
            "deny" | "block" | "no" => Self::Deny,
            _ => Self::Ask,
        }
    }

    /// Settings label.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ask => "ask",
            Self::Allow => "allow",
            Self::Deny => "deny",
        }
    }
}

/// Default per-tool permissions (others can be added in settings).
pub fn default_tool_permissions() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("write".into(), "ask".into()),
        ("edit".into(), "ask".into()),
        ("bash".into(), "ask".into()),
        ("read".into(), "allow".into()),
        ("web_fetch".into(), "ask".into()),
        ("web_search".into(), "ask".into()),
    ])
}

/// Fill in permissions added by Loop after a settings file was created.
pub fn merge_default_tool_permissions(map: &mut BTreeMap<String, String>) {
    for (tool, permission) in default_tool_permissions() {
        map.entry(tool).or_insert(permission);
    }
}

/// Tools that currently have an interactive approval workflow.
pub fn interactive_tools() -> &'static [&'static str] {
    &["write", "edit", "bash", "web_fetch", "web_search"]
}

/// Map a tool name to its session auto-approve group.
pub fn approval_group(tool: &str) -> Option<&'static str> {
    match tool {
        "write" | "edit" => Some(GROUP_FILE),
        "bash" => Some(GROUP_BASH),
        "web_fetch" | "web_search" => Some(GROUP_WEB),
        _ => None,
    }
}

/// User decision for a pending approval.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalDecision {
    /// Accept this one call.
    Accept,
    /// Accept this and all further calls in the same group for this session.
    AcceptSession,
    /// Reject; optional reason guides the model.
    Reject { reason: Option<String> },
}

/// Kind of approval prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalKind {
    /// After write/edit — show diff, can revert.
    FileEdit,
    /// Before bash — approve the command.
    Bash,
    /// Before web_fetch / web_search — approve the outbound request.
    Web,
}

impl ApprovalKind {
    /// Short label for the picker.
    pub fn label(self) -> &'static str {
        match self {
            Self::FileEdit => "file edit",
            Self::Bash => "bash",
            Self::Web => "web",
        }
    }

    /// Accept-all option text.
    pub fn accept_all_label(self) -> &'static str {
        match self {
            Self::FileEdit => "Accept all file changes for this session",
            Self::Bash => "Accept all bash for this session",
            Self::Web => "Accept all web requests for this session",
        }
    }

    /// Session auto-approve group.
    pub fn group(self) -> &'static str {
        match self {
            Self::FileEdit => GROUP_FILE,
            Self::Bash => GROUP_BASH,
            Self::Web => GROUP_WEB,
        }
    }
}

/// Prompt shown in the TUI while the agent waits.
#[derive(Debug)]
pub struct ApprovalPrompt {
    /// File edit vs bash (extensible).
    pub kind: ApprovalKind,
    /// Tool name.
    pub tool_name: String,
    /// Primary display line (path or command summary).
    pub summary: String,
    /// Optional detail (full command, absolute path).
    pub detail: String,
    /// Reply channel.
    pub response_tx: oneshot::Sender<ApprovalDecision>,
}

/// Shared bridge between agent hooks and the TUI.
#[derive(Clone)]
pub struct ToolApprovalBridge {
    /// Whether the approval policy is active for this session.
    policy_active: Arc<std::sync::atomic::AtomicBool>,
    /// Per-tool permission overrides from settings.
    permissions: Arc<SyncMutex<BTreeMap<String, ToolPermission>>>,
    /// Session auto-approve groups (`file`, `bash`, …).
    auto_approve: Arc<SyncMutex<HashSet<String>>>,
    prompts: PromptSink,
    env: Arc<dyn ExecutionEnv>,
    diff_editor: Arc<Mutex<Option<String>>>,
    /// When false, skip spawning an external `--diff` editor (desktop reviews in-app).
    open_external_diff: Arc<AtomicBool>,
    gate: Arc<Mutex<()>>,
    /// Persist auto-approve labels onto the session (optional).
    persist: Arc<SyncMutex<Option<PersistAutoApprove>>>,
}

/// Where approval prompts go.
#[derive(Clone)]
enum PromptSink {
    /// Raw prompts for a surface that renders [`ApprovalPrompt`] itself.
    Prompts(mpsc::UnboundedSender<ApprovalPrompt>),
    /// Interaction requests through Loop's [`HostServices`], shared with
    /// every other backend.
    Host(Arc<dyn HostServices>),
}

/// Callback used to append a session label (and optionally notify).
pub type PersistAutoApprove = Arc<
    dyn Fn(String) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync,
>;

impl ToolApprovalBridge {
    /// Create a bridge; `prompt_rx` is drained by the TUI.
    pub fn new(
        env: Arc<dyn ExecutionEnv>,
        diff_editor: Option<String>,
        policy_active: bool,
        permissions: BTreeMap<String, ToolPermission>,
        session_auto_approve: HashSet<String>,
    ) -> (Self, mpsc::UnboundedReceiver<ApprovalPrompt>) {
        let (prompt_tx, prompt_rx) = mpsc::unbounded_channel();
        (
            Self::with_sink(
                env,
                diff_editor,
                policy_active,
                permissions,
                session_auto_approve,
                PromptSink::Prompts(prompt_tx),
            ),
            prompt_rx,
        )
    }

    /// Create a bridge that asks through `host` as
    /// [`InteractionRequest::Permission`](loop_protocol::InteractionRequest)s
    /// with the options in [`crate::interaction::native_options`].
    pub fn with_host(
        env: Arc<dyn ExecutionEnv>,
        diff_editor: Option<String>,
        policy_active: bool,
        permissions: BTreeMap<String, ToolPermission>,
        session_auto_approve: HashSet<String>,
        host: Arc<dyn HostServices>,
    ) -> Self {
        Self::with_sink(
            env,
            diff_editor,
            policy_active,
            permissions,
            session_auto_approve,
            PromptSink::Host(host),
        )
    }

    fn with_sink(
        env: Arc<dyn ExecutionEnv>,
        diff_editor: Option<String>,
        policy_active: bool,
        permissions: BTreeMap<String, ToolPermission>,
        session_auto_approve: HashSet<String>,
        prompts: PromptSink,
    ) -> Self {
        Self {
            policy_active: Arc::new(std::sync::atomic::AtomicBool::new(policy_active)),
            permissions: Arc::new(SyncMutex::new(permissions)),
            auto_approve: Arc::new(SyncMutex::new(session_auto_approve)),
            prompts,
            env,
            diff_editor: Arc::new(Mutex::new(diff_editor)),
            open_external_diff: Arc::new(AtomicBool::new(true)),
            gate: Arc::new(Mutex::new(())),
            persist: Arc::new(SyncMutex::new(None)),
        }
    }

    /// Whether file-edit review should spawn an external editor with `--diff`.
    /// Desktop disables this and shows diffs in its own panel instead.
    pub fn set_open_external_diff(&self, open: bool) {
        self.open_external_diff.store(open, Ordering::SeqCst);
    }

    /// Install a persistence callback for Accept-all labels.
    pub fn set_persist(&self, persist: PersistAutoApprove) {
        *self.persist.lock() = Some(persist);
    }

    /// Enable/disable ask policy for this session.
    pub fn set_policy_active(&self, active: bool) {
        self.policy_active
            .store(active, std::sync::atomic::Ordering::SeqCst);
    }

    /// Whether ask policy is active.
    pub fn policy_active(&self) -> bool {
        self.policy_active.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Replace per-tool permissions from settings.
    pub fn set_permissions(&self, permissions: BTreeMap<String, ToolPermission>) {
        *self.permissions.lock() = permissions;
    }

    /// Snapshot of session auto-approve groups.
    pub fn auto_approve_groups(&self) -> HashSet<String> {
        self.auto_approve.lock().clone()
    }

    /// Mark a group as auto-approved for this session (in-memory).
    pub fn grant_session(&self, group: &str) {
        self.auto_approve.lock().insert(group.to_string());
    }

    /// Clear session auto-approve (e.g. `/new`).
    pub fn clear_session_grants(&self) {
        self.auto_approve.lock().clear();
    }

    /// Update preferred diff editor.
    pub async fn set_diff_editor(&self, editor: Option<String>) {
        *self.diff_editor.lock().await = editor;
    }

    /// Resolve permission for a tool (defaults to allow for unknown tools).
    pub fn permission_for(&self, tool: &str) -> ToolPermission {
        self.permissions
            .lock()
            .get(tool)
            .copied()
            .unwrap_or(ToolPermission::Allow)
    }

    /// Whether we should interactively ask for this tool right now.
    pub fn should_ask(&self, tool: &str) -> bool {
        if !interactive_tools().contains(&tool) {
            return false;
        }
        match self.permission_for(tool) {
            ToolPermission::Allow => false,
            ToolPermission::Deny => false, // handled as hard block separately
            ToolPermission::Ask => {
                if let Some(group) = approval_group(tool) {
                    if self.auto_approve.lock().contains(group) {
                        return false;
                    }
                }
                self.policy_active()
            }
        }
    }

    /// Whether the tool is hard-denied by config.
    pub fn is_denied(&self, tool: &str) -> bool {
        self.permission_for(tool) == ToolPermission::Deny
    }

    /// Hook for [`AgentHarness::set_after_tool_call`] (file edits).
    pub fn after_tool_hook(self: &Arc<Self>) -> loop_agent::AfterToolCallFn {
        let bridge = Arc::clone(self);
        Arc::new(move |ctx, cancel| {
            let bridge = Arc::clone(&bridge);
            Box::pin(async move { bridge.review_after_file_tool(ctx, cancel).await })
        })
    }

    /// Hook for [`AgentHarness::set_before_tool_call`] (bash + deny).
    pub fn before_tool_hook(self: &Arc<Self>) -> loop_agent::BeforeToolCallFn {
        let bridge = Arc::clone(self);
        Arc::new(move |ctx, cancel| {
            let bridge = Arc::clone(&bridge);
            Box::pin(async move { bridge.review_before_tool(ctx, cancel).await })
        })
    }

    async fn review_before_tool(
        &self,
        ctx: BeforeToolCallContext,
        cancel: Option<CancellationToken>,
    ) -> Option<BeforeToolCallResult> {
        let name = ctx.tool_call.name.as_str();
        if self.is_denied(name) {
            return Some(BeforeToolCallResult {
                block: true,
                reason: Some(format!(
                    "Tool `{name}` is denied by settings.toolPermissions"
                )),
            });
        }
        if !self.should_ask(name) {
            return None;
        }
        // Build the (kind, summary, detail, reject subject) tuple for the
        // prompt; only tools with an interactive workflow get here
        // (`should_ask` filters the rest).
        let (kind, summary, detail, subject) = match name {
            "bash" => {
                let command = ctx
                    .args
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let cwd = ctx
                    .args
                    .get("cwd")
                    .and_then(|v| v.as_str())
                    .unwrap_or(".")
                    .to_string();
                let summary = truncate_cmd(&command, 72);
                let detail = if cwd == "." {
                    command.clone()
                } else {
                    format!("cwd: {cwd}\n{command}")
                };
                (
                    ApprovalKind::Bash,
                    summary,
                    detail,
                    "bash command".to_string(),
                )
            }
            "web_fetch" => {
                let url = ctx
                    .args
                    .get("url")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let summary = truncate_cmd(&url, 72);
                (
                    ApprovalKind::Web,
                    summary,
                    url.clone(),
                    format!("fetch of {url}"),
                )
            }
            "web_search" => {
                let query = ctx
                    .args
                    .get("query")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let summary = truncate_cmd(&query, 72);
                (
                    ApprovalKind::Web,
                    summary,
                    query.clone(),
                    format!("search for '{query}'"),
                )
            }
            _ => return None,
        };

        let _lock = self.gate.lock().await;
        // Re-check after waiting for the gate (Accept-all may have landed).
        if !self.should_ask(name) {
            return None;
        }
        if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
            return Some(BeforeToolCallResult {
                block: true,
                reason: Some("Approval cancelled".into()),
            });
        }

        let decision = self
            .ask_user(kind, name, summary, detail, cancel.as_ref())
            .await;

        match decision {
            ApprovalDecision::Accept => None,
            ApprovalDecision::AcceptSession => {
                self.grant_and_persist(kind.group()).await;
                None
            }
            ApprovalDecision::Reject { reason } => {
                let msg = match reason.filter(|r| !r.trim().is_empty()) {
                    Some(r) => format!("User rejected {subject}. Reason: {r}"),
                    None => format!("User rejected {subject}."),
                };
                Some(BeforeToolCallResult {
                    block: true,
                    reason: Some(msg),
                })
            }
        }
    }

    async fn review_after_file_tool(
        &self,
        ctx: AfterToolCallContext,
        cancel: Option<CancellationToken>,
    ) -> Option<AfterToolCallResult> {
        if ctx.is_error {
            return None;
        }
        let name = ctx.tool_call.name.as_str();
        if name != "write" && name != "edit" {
            return None;
        }
        if self.is_denied(name) {
            // Should have been blocked before; if we got here, revert.
            return self
                .reject_file_result(&ctx, "Tool denied by settings.toolPermissions")
                .await;
        }
        if !self.should_ask(name) {
            // Still clean up review snapshots when not asking.
            if let Some(prev) = ctx
                .result
                .details
                .get("previousPath")
                .and_then(|v| v.as_str())
            {
                cleanup_snapshot(Some(Path::new(prev)));
            }
            return None;
        }

        let path = ctx
            .result
            .details
            .get("path")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)?;
        let previous_path = ctx
            .result
            .details
            .get("previousPath")
            .and_then(|v| v.as_str())
            .map(PathBuf::from);
        let created = ctx
            .result
            .details
            .get("created")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let _lock = self.gate.lock().await;
        if !self.should_ask(name) {
            cleanup_snapshot(previous_path.as_deref());
            return None;
        }
        if cancel.as_ref().is_some_and(|c| c.is_cancelled()) {
            let _ = self
                .revert_change(&path, previous_path.as_deref(), created)
                .await;
            return Some(reject_file_patch(
                "File edit review cancelled",
                &path,
                previous_path.as_deref(),
            ));
        }

        if self.open_external_diff.load(Ordering::SeqCst) {
            if let Some(prev) = &previous_path {
                if let Some(editor) = self.resolve_editor().await {
                    if let Err(e) = open_diff(&editor, prev, &path) {
                        tracing::warn!("failed to open diff editor ({editor}): {e}");
                    }
                } else {
                    tracing::warn!(
                        "no diff editor found (set settings.diffEditor or install cursor/code)"
                    );
                }
            }
        }

        let display = path
            .file_name()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| path.display().to_string());

        let decision = self
            .ask_user(
                ApprovalKind::FileEdit,
                name,
                display,
                path.display().to_string(),
                cancel.as_ref(),
            )
            .await;

        match decision {
            ApprovalDecision::Accept => {
                cleanup_snapshot(previous_path.as_deref());
                None
            }
            ApprovalDecision::AcceptSession => {
                cleanup_snapshot(previous_path.as_deref());
                self.grant_and_persist(ApprovalKind::FileEdit.group()).await;
                None
            }
            ApprovalDecision::Reject { reason } => {
                if let Err(e) = self
                    .revert_change(&path, previous_path.as_deref(), created)
                    .await
                {
                    tracing::warn!("revert after reject failed: {e}");
                }
                cleanup_snapshot(previous_path.as_deref());
                let msg = match reason.filter(|r| !r.trim().is_empty()) {
                    Some(r) => format!("User rejected the edit to {}. Reason: {r}", path.display()),
                    None => format!("User rejected the edit to {}.", path.display()),
                };
                Some(reject_file_patch(&msg, &path, None))
            }
        }
    }

    async fn reject_file_result(
        &self,
        ctx: &AfterToolCallContext,
        message: &str,
    ) -> Option<AfterToolCallResult> {
        let path = ctx
            .result
            .details
            .get("path")
            .and_then(|v| v.as_str())
            .map(PathBuf::from)?;
        let previous_path = ctx
            .result
            .details
            .get("previousPath")
            .and_then(|v| v.as_str())
            .map(PathBuf::from);
        let created = ctx
            .result
            .details
            .get("created")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let _ = self
            .revert_change(&path, previous_path.as_deref(), created)
            .await;
        Some(reject_file_patch(message, &path, previous_path.as_deref()))
    }

    async fn ask_user(
        &self,
        kind: ApprovalKind,
        tool_name: &str,
        summary: String,
        detail: String,
        cancel: Option<&CancellationToken>,
    ) -> ApprovalDecision {
        let response_rx = match &self.prompts {
            PromptSink::Prompts(tx) => {
                let (response_tx, response_rx) = oneshot::channel();
                if tx
                    .send(ApprovalPrompt {
                        kind,
                        tool_name: tool_name.to_string(),
                        summary,
                        detail,
                        response_tx,
                    })
                    .is_err()
                {
                    return ApprovalDecision::Reject {
                        reason: Some("approval UI unavailable".into()),
                    };
                }
                response_rx
            }
            PromptSink::Host(host) => {
                let request = native_permission_request(kind, tool_name, summary, detail);
                let host = Arc::clone(host);
                let (response_tx, response_rx) = oneshot::channel();
                tokio::spawn(async move {
                    let _ = response_tx.send(native_decision(host.interact(request).await));
                });
                response_rx
            }
        };

        tokio::select! {
            biased;
            _ = async {
                if let Some(c) = cancel {
                    c.cancelled().await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => ApprovalDecision::Reject { reason: Some("approval cancelled".into()) },
            res = response_rx => res.unwrap_or(ApprovalDecision::Reject {
                reason: Some("approval UI closed".into()),
            }),
        }
    }

    async fn grant_and_persist(&self, group: &str) {
        self.grant_session(group);
        let label = format!("{AUTO_APPROVE_LABEL_PREFIX}{group}");
        let persist = self.persist.lock().clone();
        if let Some(persist) = persist {
            persist(label).await;
        }
    }

    async fn revert_change(
        &self,
        path: &Path,
        previous_path: Option<&Path>,
        created: bool,
    ) -> Result<(), String> {
        if created {
            let _ = self.env.remove(path).await;
            return Ok(());
        }
        let Some(prev) = previous_path else {
            return Err("missing previous snapshot".into());
        };
        let bytes = tokio::fs::read(prev).await.map_err(|e| e.to_string())?;
        self.env
            .write_file(path, &bytes)
            .await
            .map_err(|e| e.to_string())
    }

    async fn resolve_editor(&self) -> Option<String> {
        if let Some(custom) = self.diff_editor.lock().await.clone() {
            if !custom.trim().is_empty() {
                return Some(custom);
            }
        }
        detect_diff_editor()
    }
}

/// Parse auto-approve groups from session tree labels.
pub fn auto_approve_from_entries<'a>(
    entries: impl IntoIterator<Item = &'a loop_agent::harness::SessionTreeEntry>,
) -> HashSet<String> {
    use loop_agent::harness::SessionTreeEntry;
    let mut out = HashSet::new();
    for e in entries {
        if let SessionTreeEntry::Label { label, .. } = e {
            if let Some(group) = label.strip_prefix(AUTO_APPROVE_LABEL_PREFIX) {
                if !group.is_empty() {
                    out.insert(group.to_string());
                }
            }
        }
    }
    out
}

/// Build permission map from settings JSON strings.
pub fn permissions_from_settings(
    map: &BTreeMap<String, String>,
) -> BTreeMap<String, ToolPermission> {
    let mut merged = map.clone();
    merge_default_tool_permissions(&mut merged);
    let mut out = BTreeMap::new();
    for (k, v) in merged {
        out.insert(k, ToolPermission::parse(&v));
    }
    out
}

fn reject_file_patch(
    message: &str,
    path: &Path,
    previous_path: Option<&Path>,
) -> AfterToolCallResult {
    cleanup_snapshot(previous_path);
    AfterToolCallResult {
        content: Some(vec![ToolResultContent::Text(TextContent {
            text: message.to_string(),
            text_signature: None,
        })]),
        details: Some(json!({
            "path": path,
            "rejected": true,
        })),
        is_error: Some(true),
        usage: None,
        terminate: None,
    }
}

fn cleanup_snapshot(path: Option<&Path>) {
    if let Some(p) = path {
        let _ = std::fs::remove_file(p);
    }
}

fn truncate_cmd(cmd: &str, max: usize) -> String {
    let one = cmd.replace('\n', " ");
    if one.chars().count() <= max {
        return one;
    }
    let mut out: String = one.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Prefer Cursor, then VS Code-style binaries.
pub fn detect_diff_editor() -> Option<String> {
    for candidate in ["cursor", "code", "code-insiders", "codium"] {
        if which_ok(candidate) {
            return Some(candidate.to_string());
        }
    }
    None
}

fn which_ok(bin: &str) -> bool {
    Command::new("which")
        .arg(bin)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Open a side-by-side diff (`--diff left right`).
pub fn open_diff(editor: &str, left: &Path, right: &Path) -> Result<(), String> {
    Command::new(editor)
        .arg("--diff")
        .arg(left)
        .arg(right)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn {editor}: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_and_permissions() {
        assert!(ApprovalPolicy::NewSession.asks_for_session(true));
        assert!(!ApprovalPolicy::NewSession.asks_for_session(false));
        assert_eq!(ToolPermission::parse("ASK"), ToolPermission::Ask);
        assert_eq!(approval_group("write"), Some(GROUP_FILE));
        assert_eq!(approval_group("bash"), Some(GROUP_BASH));
    }

    #[test]
    fn legacy_permission_maps_get_new_web_defaults() {
        let permissions =
            permissions_from_settings(&BTreeMap::from([("bash".into(), "allow".into())]));
        assert_eq!(permissions.get("bash"), Some(&ToolPermission::Allow));
        assert_eq!(permissions.get("web_fetch"), Some(&ToolPermission::Ask));
        assert_eq!(permissions.get("web_search"), Some(&ToolPermission::Ask));
    }

    #[test]
    fn auto_approve_label_roundtrip() {
        use loop_agent::harness::SessionTreeEntry;
        let entries = vec![SessionTreeEntry::Label {
            id: "1".into(),
            parent_id: None,
            timestamp: 0,
            label: format!("{AUTO_APPROVE_LABEL_PREFIX}{GROUP_FILE}"),
        }];
        let set = auto_approve_from_entries(&entries);
        assert!(set.contains(GROUP_FILE));
    }
}
