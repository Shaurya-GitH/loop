//! Bridges `AgentHarness` events to the GPUI UI thread.

use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_channel::{Receiver, Sender};
use loop_agent::harness::types::ShellExecOptions;
use loop_agent::harness::{
    create_session_repository, create_sqlite_session_store, AgentHarnessPhase,
};
use loop_agent::types::{AgentEvent, AgentMessage, AgentThinkingLevel, AgentToolResult};
use loop_ai::AssistantMessageEvent;
use loop_app_core::tool_approval::{
    permissions_from_settings, ApprovalDecision, ToolApprovalBridge,
};
use loop_app_core::{bootstrap, settings_path, BootstrapOpts, Runtime, Settings};
use parking_lot::Mutex;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::approval::{ApprovalUiPrompt, PendingApproval};
use crate::state::{ChatRow, ComposerStats, PendingFileChange, SessionRow, ToolCardStatus};

/// Pre-edit snapshot captured at tool-start (paths resolved against project cwd).
#[derive(Debug, Clone)]
struct FileEditSnapshot {
    path: PathBuf,
    before: Option<String>,
    /// `write` args `content` — used when the on-disk read is unavailable.
    after_hint: Option<String>,
}

/// UI-facing agent command.
#[derive(Debug)]
pub enum DesktopCommand {
    Prompt(String),
    /// Abort the in-flight agent turn and roll UI back as if it never ran.
    Stop,
    /// Drop the most recently queued follow-up.
    DequeueLast,
    /// Drop every queued follow-up.
    ClearQueue,
    SetModel {
        provider: String,
        model_id: String,
    },
    CycleModel,
    SetThinking(AgentThinkingLevel),
    CycleThinking,
    SelectFileChange(String),
    AcceptFileChange(String),
    RejectFileChange(String),
    ResolveApproval {
        accept: bool,
        session: bool,
        reason: Option<String>,
    },
    OpenInEditor,
    RefreshSessions,
    SelectSession(String),
    NewSession,
}

/// Checkpoint taken at the start of a prompt so Stop can unwind the turn.
#[derive(Debug, Clone)]
struct TurnCheckpoint {
    text: String,
    chat_len: usize,
    pending_change_ids: Vec<String>,
}

/// Snapshot pushed to the GPUI entity after each controller update.
#[derive(Debug, Clone)]
pub struct DesktopSnapshot {
    pub chat_rows: Vec<ChatRow>,
    pub sessions: Vec<SessionRow>,
    pub pending_changes: Vec<PendingFileChange>,
    pub selected_change_id: Option<String>,
    pub stats: ComposerStats,
    pub streaming: bool,
    pub phase: AgentHarnessPhase,
    pub active_session_id: String,
    pub cwd: PathBuf,
    pub model_label: String,
    pub thinking_label: String,
    pub available_models: Vec<(String, String, String)>,
    pub approval_prompt: Option<ApprovalUiPrompt>,
    pub detected_editors: Vec<String>,
    /// When set, the composer should take this text (Stop restoring the prompt).
    pub restore_composer: Option<String>,
    /// True while an agent Prompt is in flight (not bang-shell).
    pub agent_turn: bool,
    /// Follow-up prompts waiting until the current turn finishes (FIFO).
    pub queued_messages: VecDeque<String>,
}

/// Background controller owning the shared runtime.
pub struct DesktopController {
    runtime: Arc<Runtime>,
    event_rx: Receiver<()>,
    ui_tick_tx: Sender<()>,
    snapshot: Arc<Mutex<DesktopSnapshot>>,
    pending_approval: Arc<Mutex<Option<PendingApproval>>>,
    /// Pre-write file contents keyed by tool call id (absolute path + optional after hint).
    file_snapshots: Arc<Mutex<HashMap<String, FileEditSnapshot>>>,
    model_index: Arc<Mutex<usize>>,
    thinking_levels: Vec<AgentThinkingLevel>,
    thinking_index: Arc<Mutex<usize>>,
    /// In-flight prompt checkpoint for Stop rollback.
    turn_checkpoint: Arc<Mutex<Option<TurnCheckpoint>>>,
    /// When true, discard the current turn after abort and ignore late agent events.
    discard_turn: Arc<Mutex<bool>>,
    /// Drop agent events until the next Prompt starts (after Stop).
    suppress_events: Arc<std::sync::atomic::AtomicBool>,
    /// Serialize agent prompts so a resubmit after Stop cannot race a still-finishing turn.
    prompt_lock: tokio::sync::Mutex<()>,
}

impl DesktopController {
    pub async fn new(cwd: PathBuf) -> anyhow::Result<Self> {
        let runtime = Arc::new(
            bootstrap(BootstrapOpts {
                cwd: cwd.clone(),
                provider: None,
                model: None,
                theme: None,
                system_prompt: None,
                append_system_prompt: None,
                no_context_files: false,
                no_web: false,
                interactive: false,
                session_id: None,
                agent: None,
            })
            .await?,
        );

        let (ui_tick_tx, event_rx) = async_channel::unbounded();
        let stats = ComposerStats::from_runtime(&runtime);
        let model_label = model_display(&runtime);
        let thinking_label = runtime.settings.default_thinking_level.clone();
        let available_models = list_models(&runtime);
        let detected_editors = crate::editor_launcher::detect_editors()
            .into_iter()
            .map(|e| e.label().to_string())
            .collect();

        let snapshot = Arc::new(Mutex::new(DesktopSnapshot {
            chat_rows: Vec::new(),
            sessions: Vec::new(),
            pending_changes: Vec::new(),
            selected_change_id: None,
            stats,
            streaming: false,
            phase: AgentHarnessPhase::Idle,
            active_session_id: runtime.session_id.clone(),
            cwd: cwd.clone(),
            model_label,
            thinking_label: thinking_label.clone(),
            available_models: available_models.clone(),
            approval_prompt: None,
            detected_editors,
            restore_composer: None,
            agent_turn: false,
            queued_messages: VecDeque::new(),
        }));

        let tool_env = runtime.harness.tool_env().await?;
        let policy_active =
            loop_app_core::tool_approval::ApprovalPolicy::parse(&runtime.settings.file_edit_review)
                .asks_for_session(!runtime.resumed);
        let (bridge, approval_rx) = ToolApprovalBridge::new(
            tool_env,
            runtime.settings.diff_editor.clone(),
            policy_active,
            permissions_from_settings(&runtime.settings.tool_permissions),
            Default::default(),
        );
        // Review file edits in the desktop diff panel; do not spawn Cursor/VS Code on each write.
        bridge.set_open_external_diff(false);
        let bridge = Arc::new(bridge);
        runtime.tool_gate.set_approver(Some(Arc::clone(&bridge)));

        let thinking_levels = vec![
            AgentThinkingLevel::Off,
            AgentThinkingLevel::Minimal,
            AgentThinkingLevel::Low,
            AgentThinkingLevel::Medium,
            AgentThinkingLevel::High,
            AgentThinkingLevel::XHigh,
            AgentThinkingLevel::Max,
        ];
        let thinking_index = thinking_levels
            .iter()
            .position(|l| thinking_label == thinking_label_str(*l))
            .unwrap_or(0);

        let controller = Self {
            runtime: Arc::clone(&runtime),
            event_rx,
            ui_tick_tx,
            snapshot,
            pending_approval: Arc::new(Mutex::new(None)),
            file_snapshots: Arc::new(Mutex::new(HashMap::new())),
            model_index: Arc::new(Mutex::new(0)),
            thinking_levels,
            thinking_index: Arc::new(Mutex::new(thinking_index)),
            turn_checkpoint: Arc::new(Mutex::new(None)),
            discard_turn: Arc::new(Mutex::new(false)),
            suppress_events: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            prompt_lock: tokio::sync::Mutex::new(()),
        };

        controller.subscribe_agent_events();
        controller.spawn_approval_handler(approval_rx);
        controller.spawn_model_refresh();
        let _ = crate::session_title::backfill_untitled_sessions(&controller.runtime).await;
        controller.refresh_sessions().await?;
        controller.hydrate_transcript().await?;
        controller.refresh_stats().await?;

        Ok(controller)
    }

    pub fn snapshot(&self) -> DesktopSnapshot {
        self.snapshot.lock().clone()
    }

    pub fn ui_receiver(&self) -> Receiver<()> {
        self.event_rx.clone()
    }

    pub fn runtime(&self) -> Arc<Runtime> {
        Arc::clone(&self.runtime)
    }

    /// Preferred UI theme name from settings (`dark` / `light`).
    pub fn theme_name(&self) -> &str {
        &self.runtime.settings.theme
    }

    /// Persist the desktop/CLI theme preference to `~/.loop/agent/settings.json`.
    pub fn persist_theme(&self, name: &str) {
        let path = settings_path(&self.runtime.agent_dir);
        match Settings::load_file(&path) {
            Ok(mut settings) => {
                settings.theme = name.to_string();
                if let Err(error) = settings.save_file(&path) {
                    tracing::warn!("failed to save theme setting: {error:#}");
                }
            }
            Err(error) => tracing::warn!("failed to load settings for theme save: {error:#}"),
        }
    }

    fn notify_ui(&self) {
        let _ = self.ui_tick_tx.try_send(());
    }

    fn subscribe_agent_events(&self) {
        let snap = Arc::clone(&self.snapshot);
        let file_snapshots = Arc::clone(&self.file_snapshots);
        let ui_tick = self.ui_tick_tx.clone();
        let suppress = Arc::clone(&self.suppress_events);

        self.runtime.harness.subscribe(move |event| {
            let snap = Arc::clone(&snap);
            let file_snapshots = Arc::clone(&file_snapshots);
            let ui_tick = ui_tick.clone();
            let suppress = Arc::clone(&suppress);
            async move {
                if suppress.load(std::sync::atomic::Ordering::Relaxed) {
                    if matches!(event, AgentEvent::AgentEnd { .. }) {
                        let mut s = snap.lock();
                        s.streaming = false;
                        s.phase = AgentHarnessPhase::Idle;
                    }
                    let _ = ui_tick.try_send(());
                    return;
                }
                apply_agent_event(&snap, &file_snapshots, event);
                let _ = ui_tick.try_send(());
            }
        });
    }

    fn spawn_approval_handler(
        &self,
        mut approval_rx: mpsc::UnboundedReceiver<loop_app_core::tool_approval::ApprovalPrompt>,
    ) {
        let snap = Arc::clone(&self.snapshot);
        let pending = Arc::clone(&self.pending_approval);
        let ui_tick = self.ui_tick_tx.clone();
        tokio::spawn(async move {
            while let Some(prompt) = approval_rx.recv().await {
                {
                    let mut s = snap.lock();
                    s.approval_prompt = Some(ApprovalUiPrompt::from_prompt(&prompt));
                }
                *pending.lock() = Some(PendingApproval { prompt });
                let _ = ui_tick.send(()).await;
            }
        });
    }

    fn spawn_model_refresh(&self) {
        let runtime = Arc::clone(&self.runtime);
        let snapshot = Arc::clone(&self.snapshot);
        let ui_tick = self.ui_tick_tx.clone();
        tokio::spawn(async move {
            use loop_ai::providers::SOKET_PROVIDER_ID;
            use loop_ai::ModelsRefreshOptions;
            let refresh = runtime
                .models
                .refresh(ModelsRefreshOptions {
                    allow_network: Some(true),
                    force: false,
                    provider_id: Some(SOKET_PROVIDER_ID.into()),
                })
                .await;
            for (pid, err) in &refresh.errors {
                tracing::warn!("model refresh {pid}: {err}");
            }
            snapshot.lock().available_models = list_models(&runtime);
            let _ = ui_tick.try_send(());
        });
    }

    pub async fn handle_command(&self, cmd: DesktopCommand) -> anyhow::Result<()> {
        match cmd {
            DesktopCommand::Prompt(text) => {
                if text.trim().is_empty() {
                    return Ok(());
                }
                if let Some(command) = text.strip_prefix('!') {
                    return self.run_bang_command(&text, command).await;
                }

                // Enqueue without waiting on the prompt lock so the composer can
                // show "N queued" immediately while a turn is in flight.
                if self.should_queue_prompt() {
                    self.enqueue_prompt(text);
                    return Ok(());
                }

                let _prompt_guard = self.prompt_lock.lock().await;
                // Re-check under the lock — a turn may have started between the
                // fast-path check and acquiring the lock.
                if self.should_queue_prompt() {
                    self.enqueue_prompt(text);
                    return Ok(());
                }

                // Preserve FIFO: if follow-ups were left over, append this message
                // and drain from the front.
                let mut next = if self.snapshot.lock().queued_messages.is_empty() {
                    text
                } else {
                    self.enqueue_prompt(text);
                    self.pop_queued_prompt().expect("queue non-empty")
                };
                loop {
                    self.run_prompt_turn(next).await?;
                    let Some(queued) = self.pop_queued_prompt() else {
                        break;
                    };
                    next = queued;
                }
            }
            DesktopCommand::Stop => {
                self.clear_queued_prompts();
                self.stop_turn().await;
            }
            DesktopCommand::DequeueLast => {
                {
                    let mut snap = self.snapshot.lock();
                    snap.queued_messages.pop_back();
                }
                self.notify_ui();
            }
            DesktopCommand::ClearQueue => {
                self.clear_queued_prompts();
            }
            DesktopCommand::SetModel { provider, model_id } => {
                if let Some(model) = self.runtime.models.get_model(&provider, &model_id) {
                    self.runtime.harness.set_model(model).await;
                    let mut snap = self.snapshot.lock();
                    snap.model_label = format!("{provider}/{model_id}");
                    snap.available_models = list_models(&self.runtime);
                }
                self.refresh_stats().await?;
            }
            DesktopCommand::CycleModel => {
                let models = list_models(&self.runtime);
                if models.is_empty() {
                    return Ok(());
                }
                let idx = {
                    let mut i = self.model_index.lock();
                    *i = (*i + 1) % models.len();
                    *i
                };
                let (provider, model_id, _) = &models[idx];
                Box::pin(self.handle_command(DesktopCommand::SetModel {
                    provider: provider.clone(),
                    model_id: model_id.clone(),
                }))
                .await?;
            }
            DesktopCommand::SetThinking(level) => {
                self.runtime.harness.set_thinking_level(level).await;
                let mut snap = self.snapshot.lock();
                snap.thinking_label = thinking_label_str(level);
                if let Some(ix) = self.thinking_levels.iter().position(|l| *l == level) {
                    *self.thinking_index.lock() = ix;
                }
            }
            DesktopCommand::CycleThinking => {
                let idx = {
                    let mut i = self.thinking_index.lock();
                    *i = (*i + 1) % self.thinking_levels.len();
                    *i
                };
                let level = self.thinking_levels[idx];
                Box::pin(self.handle_command(DesktopCommand::SetThinking(level))).await?;
            }
            DesktopCommand::SelectFileChange(id) => {
                self.snapshot.lock().selected_change_id = Some(id);
                self.notify_ui();
            }
            DesktopCommand::AcceptFileChange(id) => {
                let mut snap = self.snapshot.lock();
                if let Some(c) = snap.pending_changes.iter_mut().find(|c| c.id == id) {
                    c.reviewed = true;
                }
                self.notify_ui();
            }
            DesktopCommand::RejectFileChange(id) => {
                let change = {
                    let snap = self.snapshot.lock();
                    snap.pending_changes.iter().find(|c| c.id == id).cloned()
                };
                if let Some(change) = change {
                    crate::state::reject_change(&change)?;
                    let mut snap = self.snapshot.lock();
                    snap.pending_changes.retain(|c| c.id != id);
                    if snap.selected_change_id.as_deref() == Some(&id) {
                        snap.selected_change_id = None;
                    }
                }
                self.notify_ui();
            }
            DesktopCommand::ResolveApproval {
                accept,
                session,
                reason,
            } => {
                if let Some(pending) = self.pending_approval.lock().take() {
                    let decision = if accept {
                        if session {
                            ApprovalDecision::AcceptSession
                        } else {
                            ApprovalDecision::Accept
                        }
                    } else {
                        ApprovalDecision::Reject { reason }
                    };
                    pending.respond(decision);
                }
                self.snapshot.lock().approval_prompt = None;
                self.notify_ui();
            }
            DesktopCommand::OpenInEditor => {
                let change = {
                    let snap = self.snapshot.lock();
                    snap.pending_changes
                        .iter()
                        .find(|c| {
                            Some(c.id.as_str()) == snap.selected_change_id.as_deref()
                                || snap.selected_change_id.is_none()
                        })
                        .or_else(|| snap.pending_changes.last())
                        .cloned()
                };
                if let Some(change) = change {
                    if let Some(editor) =
                        crate::editor_launcher::detect_editors().into_iter().next()
                    {
                        crate::editor_launcher::open_in_editor(editor, &change.path, 1)?;
                    }
                }
            }
            DesktopCommand::RefreshSessions => {
                self.refresh_sessions().await?;
            }
            DesktopCommand::SelectSession(id) => {
                if id == self.snapshot.lock().active_session_id {
                    return Ok(());
                }
                self.runtime
                    .harness
                    .switch_session(&id)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
                {
                    let mut snap = self.snapshot.lock();
                    snap.active_session_id = id;
                    snap.chat_rows.clear();
                    snap.pending_changes.clear();
                    snap.selected_change_id = None;
                    snap.streaming = false;
                    snap.agent_turn = false;
                    snap.approval_prompt = None;
                    snap.restore_composer = None;
                    snap.queued_messages.clear();
                }
                *self.pending_approval.lock() = None;
                *self.turn_checkpoint.lock() = None;
                *self.discard_turn.lock() = false;
                self.suppress_events
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                self.file_snapshots.lock().clear();
                self.hydrate_transcript().await?;
                self.refresh_sessions().await?;
                self.refresh_stats().await?;
            }
            DesktopCommand::NewSession => {
                let id = self
                    .runtime
                    .harness
                    .start_new_session(Some(self.runtime.cwd.to_string_lossy().into_owned()), None)
                    .await
                    .map_err(|e| anyhow::anyhow!(e))?;
                {
                    let mut snap = self.snapshot.lock();
                    snap.active_session_id = id;
                    snap.chat_rows.clear();
                    snap.pending_changes.clear();
                    snap.selected_change_id = None;
                    snap.streaming = false;
                    snap.agent_turn = false;
                    snap.approval_prompt = None;
                    snap.restore_composer = None;
                    snap.queued_messages.clear();
                }
                *self.pending_approval.lock() = None;
                *self.turn_checkpoint.lock() = None;
                *self.discard_turn.lock() = false;
                self.suppress_events
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                self.file_snapshots.lock().clear();
                self.refresh_sessions().await?;
                self.refresh_stats().await?;
            }
        }
        Ok(())
    }

    fn should_queue_prompt(&self) -> bool {
        let snap = self.snapshot.lock();
        snap.agent_turn
            || snap.streaming
            || snap.phase != AgentHarnessPhase::Idle
            || self.runtime.harness.phase() != AgentHarnessPhase::Idle
    }

    fn enqueue_prompt(&self, text: String) {
        {
            let mut snap = self.snapshot.lock();
            snap.queued_messages.push_back(text);
        }
        self.notify_ui();
    }

    fn pop_queued_prompt(&self) -> Option<String> {
        let mut snap = self.snapshot.lock();
        let next = snap.queued_messages.pop_front();
        if next.is_some() {
            // Release lock before notify to avoid holding it across UI wakeups.
            drop(snap);
            self.notify_ui();
            return next;
        }
        next
    }

    fn clear_queued_prompts(&self) {
        {
            let mut snap = self.snapshot.lock();
            snap.queued_messages.clear();
        }
        self.notify_ui();
    }

    /// Run a single agent turn. Caller must hold `prompt_lock` and ensure idle.
    async fn run_prompt_turn(&self, text: String) -> anyhow::Result<()> {
        // Wait out any aborted turn before starting a new one. Without this,
        // a quick Stop→resubmit races harness.prompt() and gets Busy while the
        // user bubble is already shown.
        self.runtime.harness.wait_for_idle().await;

        let needs_title = self.session_needs_title().await?;
        let fallback = crate::session_title::fallback_title(&text);
        let session_id = self.snapshot.lock().active_session_id.clone();
        {
            let mut snap = self.snapshot.lock();
            let checkpoint = TurnCheckpoint {
                text: text.clone(),
                chat_len: snap.chat_rows.len(),
                pending_change_ids: snap.pending_changes.iter().map(|c| c.id.clone()).collect(),
            };
            *self.turn_checkpoint.lock() = Some(checkpoint);
            *self.discard_turn.lock() = false;
            self.suppress_events
                .store(false, std::sync::atomic::Ordering::Relaxed);
            snap.streaming = true;
            snap.agent_turn = true;
            snap.restore_composer = None;
            snap.chat_rows.push(ChatRow::User {
                id: uuid::Uuid::now_v7().to_string(),
                text: text.clone(),
            });
            if needs_title {
                if let Some(row) = snap.sessions.iter_mut().find(|s| s.id == session_id) {
                    row.name = Some(fallback.clone());
                }
            }
        }
        self.notify_ui();
        if needs_title {
            let _ =
                crate::session_title::persist_session_name(&self.runtime, &session_id, &fallback)
                    .await;
        }
        let result = self.runtime.harness.prompt(text.clone()).await;
        let discarded = self.take_discard_and_rollback();
        {
            let mut snap = self.snapshot.lock();
            snap.streaming = false;
            snap.agent_turn = false;
            snap.phase = self.runtime.harness.phase();
        }
        self.notify_ui();
        if discarded {
            return Ok(());
        }
        if let Err(error) = result {
            // Keep the optimistic user bubble from sticking around on failure.
            self.suppress_events
                .store(true, std::sync::atomic::Ordering::Relaxed);
            self.apply_turn_rollback();
            self.notify_ui();
            return Err(anyhow::anyhow!(error));
        }
        *self.turn_checkpoint.lock() = None;
        if needs_title {
            let runtime = Arc::clone(&self.runtime);
            let snapshot = Arc::clone(&self.snapshot);
            let ui_tick = self.ui_tick_tx.clone();
            let message = text.clone();
            let (provider, model_id) = {
                let label = snapshot.lock().model_label.clone();
                split_model_label(&label)
            };
            tokio::spawn(async move {
                let title = crate::session_title::generate_session_title(
                    &runtime,
                    &message,
                    provider.as_deref(),
                    model_id.as_deref(),
                )
                .await
                .unwrap_or(fallback);
                if crate::session_title::persist_session_name(&runtime, &session_id, &title)
                    .await
                    .is_ok()
                {
                    if let Ok(rows) = load_session_rows(&runtime, &snapshot).await {
                        snapshot.lock().sessions = rows;
                        let _ = ui_tick.try_send(());
                    }
                }
            });
        }
        self.refresh_stats().await?;
        self.refresh_sessions().await?;
        Ok(())
    }

    /// Run `!command` locally: show the user bubble + output, never send to the LLM
    /// or append to the session message list.
    async fn run_bang_command(&self, raw: &str, command: &str) -> anyhow::Result<()> {
        let command = command.trim();
        {
            let mut snap = self.snapshot.lock();
            snap.chat_rows.push(ChatRow::User {
                id: uuid::Uuid::now_v7().to_string(),
                text: raw.to_string(),
            });
            if command.is_empty() {
                snap.chat_rows
                    .push(ChatRow::System("usage: !command".into()));
            } else {
                snap.streaming = true;
            }
        }
        self.notify_ui();
        if command.is_empty() {
            return Ok(());
        }

        let env = match self.runtime.harness.tool_env().await {
            Ok(env) => env,
            Err(e) => {
                {
                    let mut snap = self.snapshot.lock();
                    snap.streaming = false;
                    snap.chat_rows.push(ChatRow::Shell {
                        id: uuid::Uuid::now_v7().to_string(),
                        command: command.to_string(),
                        output: format!("shell error: {e}"),
                        exit_code: None,
                    });
                }
                self.notify_ui();
                return Ok(());
            }
        };

        let row = match env.exec(command, ShellExecOptions::default()).await {
            Ok(out) => {
                let combined = if out.stderr.is_empty() {
                    out.stdout
                } else if out.stdout.is_empty() {
                    out.stderr
                } else {
                    format!("{}\n{}", out.stdout, out.stderr)
                };
                ChatRow::Shell {
                    id: uuid::Uuid::now_v7().to_string(),
                    command: command.to_string(),
                    output: combined,
                    exit_code: Some(out.exit_code),
                }
            }
            Err(e) => ChatRow::Shell {
                id: uuid::Uuid::now_v7().to_string(),
                command: command.to_string(),
                output: format!("error: {e}"),
                exit_code: None,
            },
        };

        {
            let mut snap = self.snapshot.lock();
            snap.streaming = false;
            snap.chat_rows.push(row);
        }
        self.notify_ui();
        Ok(())
    }

    async fn refresh_stats(&self) -> anyhow::Result<()> {
        let mut stats = self.snapshot.lock().stats.clone();
        stats.refresh(&self.runtime).await;
        self.snapshot.lock().stats = stats;
        self.notify_ui();
        Ok(())
    }

    /// Abort the in-flight turn and immediately roll the UI back.
    async fn stop_turn(&self) {
        if self.turn_checkpoint.lock().is_none() {
            return;
        }
        *self.discard_turn.lock() = true;
        self.suppress_events
            .store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(pending) = self.pending_approval.lock().take() {
            pending.respond(ApprovalDecision::Reject {
                reason: Some("stopped by user".into()),
            });
        }
        {
            let mut snap = self.snapshot.lock();
            snap.approval_prompt = None;
        }
        self.runtime.harness.abort();
        self.apply_turn_rollback();
        self.notify_ui();
        // Ensure the aborted turn has fully released the harness before a
        // resubmit can start (prompt_lock alone isn't enough if Stop returns first).
        self.runtime.harness.wait_for_idle().await;
    }

    /// After `prompt()` returns, finish discard if Stop was requested.
    /// Returns true when the turn was discarded.
    ///
    /// Leaves `turn_checkpoint` in place on the non-discard path so the caller
    /// can still roll back if `harness.prompt` failed.
    fn take_discard_and_rollback(&self) -> bool {
        let discard = std::mem::replace(&mut *self.discard_turn.lock(), false);
        if !discard {
            return false;
        }
        self.suppress_events
            .store(true, std::sync::atomic::Ordering::Relaxed);
        // Stop may have already rolled the UI back; ensure idle flags.
        if self.turn_checkpoint.lock().is_some() {
            self.apply_turn_rollback();
        } else {
            let mut snap = self.snapshot.lock();
            snap.streaming = false;
            snap.agent_turn = false;
            snap.phase = AgentHarnessPhase::Idle;
        }
        true
    }

    fn apply_turn_rollback(&self) {
        let Some(checkpoint) = self.turn_checkpoint.lock().take() else {
            let mut snap = self.snapshot.lock();
            snap.streaming = false;
            snap.agent_turn = false;
            snap.phase = AgentHarnessPhase::Idle;
            snap.approval_prompt = None;
            return;
        };
        let mut snap = self.snapshot.lock();
        if snap.chat_rows.len() > checkpoint.chat_len {
            snap.chat_rows.truncate(checkpoint.chat_len);
        }
        let kept: std::collections::HashSet<_> =
            checkpoint.pending_change_ids.into_iter().collect();
        let to_revert: Vec<_> = snap
            .pending_changes
            .iter()
            .filter(|c| !kept.contains(&c.id))
            .cloned()
            .collect();
        snap.pending_changes.retain(|c| kept.contains(&c.id));
        if let Some(sel) = snap.selected_change_id.as_deref() {
            if !snap.pending_changes.iter().any(|c| c.id == sel) {
                snap.selected_change_id = None;
            }
        }
        snap.streaming = false;
        snap.agent_turn = false;
        snap.phase = AgentHarnessPhase::Idle;
        snap.approval_prompt = None;
        snap.restore_composer = Some(checkpoint.text);
        drop(snap);
        self.file_snapshots.lock().clear();
        for change in to_revert {
            if let Err(error) = crate::state::reject_change(&change) {
                tracing::warn!("failed to revert {}: {error:#}", change.path.display());
            }
        }
    }

    /// Consume a one-shot composer restore value set by Stop.
    pub fn take_restore_composer(&self) -> Option<String> {
        self.snapshot.lock().restore_composer.take()
    }

    async fn session_needs_title(&self) -> anyhow::Result<bool> {
        let store = create_sqlite_session_store(&self.runtime.sessions_db)
            .map_err(|e| anyhow::anyhow!(e))?;
        let repo = create_session_repository(store, None);
        let active = self.snapshot.lock().active_session_id.clone();
        let list = repo.list(None).await.map_err(|e| anyhow::anyhow!(e))?;
        Ok(crate::session_title::is_untitled(
            list.iter()
                .find(|m| m.id == active)
                .and_then(|m| m.name.as_deref()),
        ))
    }

    async fn refresh_sessions(&self) -> anyhow::Result<()> {
        let rows = load_session_rows(&self.runtime, &self.snapshot).await?;
        self.snapshot.lock().sessions = rows;
        self.notify_ui();
        Ok(())
    }

    async fn hydrate_transcript(&self) -> anyhow::Result<()> {
        let ctx = self
            .runtime
            .harness
            .session_context()
            .await
            .map_err(|e| anyhow::anyhow!(e))?;
        let mut rows = Vec::new();
        for msg in ctx.messages {
            rows.extend(ChatRow::from_agent_message(&msg));
        }
        self.snapshot.lock().chat_rows = rows;
        self.maybe_name_from_transcript().await?;
        self.notify_ui();
        Ok(())
    }

    async fn maybe_name_from_transcript(&self) -> anyhow::Result<()> {
        if !self.session_needs_title().await? {
            return Ok(());
        }
        let first = {
            let snap = self.snapshot.lock();
            snap.chat_rows.iter().find_map(|row| match row {
                ChatRow::User { text, .. } if !text.trim().is_empty() => Some(text.clone()),
                _ => None,
            })
        };
        let Some(text) = first else {
            return Ok(());
        };
        let title = crate::session_title::fallback_title(&text);
        let session_id = self.snapshot.lock().active_session_id.clone();
        crate::session_title::persist_session_name(&self.runtime, &session_id, &title).await?;
        if let Some(row) = self
            .snapshot
            .lock()
            .sessions
            .iter_mut()
            .find(|s| s.id == session_id)
        {
            row.name = Some(title);
        }
        Ok(())
    }
}

async fn load_session_rows(
    runtime: &Runtime,
    snapshot: &Arc<Mutex<DesktopSnapshot>>,
) -> anyhow::Result<Vec<SessionRow>> {
    let store =
        create_sqlite_session_store(&runtime.sessions_db).map_err(|e| anyhow::anyhow!(e))?;
    let repo = create_session_repository(store, None);
    let list = repo.list(None).await.map_err(|e| anyhow::anyhow!(e))?;
    let active = snapshot.lock().active_session_id.clone();
    let running = runtime.harness.phase() != AgentHarnessPhase::Idle;
    let mut rows: Vec<SessionRow> = list
        .into_iter()
        .map(|m| SessionRow {
            id: m.id.clone(),
            name: m.name.clone(),
            cwd: m.cwd.clone().unwrap_or_default(),
            updated_at: m.created_at,
            active: m.id == active,
            running: m.id == active && running,
        })
        .collect();
    rows.reverse();
    Ok(rows)
}

fn split_model_label(label: &str) -> (Option<String>, Option<String>) {
    label
        .split_once('/')
        .map(|(p, m)| (Some(p.to_string()), Some(m.to_string())))
        .unwrap_or((None, None))
}

fn apply_agent_event(
    snap: &Arc<Mutex<DesktopSnapshot>>,
    file_snapshots: &Arc<Mutex<HashMap<String, FileEditSnapshot>>>,
    event: AgentEvent,
) {
    let mut s = snap.lock();
    match &event {
        AgentEvent::AgentStart => {
            s.streaming = true;
            s.phase = AgentHarnessPhase::Turn;
        }
        AgentEvent::AgentEnd { messages } => {
            s.streaming = false;
            s.phase = AgentHarnessPhase::Idle;
            for msg in messages {
                s.stats.apply_message(msg);
            }
        }
        AgentEvent::MessageStart { message } => {
            // New assistant message: later text starts a fresh bubble (after tools/thinking).
            if message.role() == "assistant" {
                close_streaming_assistant(&mut s.chat_rows);
            }
        }
        AgentEvent::MessageUpdate {
            message,
            assistant_message_event,
        } => {
            match assistant_message_event {
                AssistantMessageEvent::TextStart { .. } => {
                    finish_open_thinking(&mut s.chat_rows);
                    // New text segment after thinking/tools.
                    close_streaming_assistant(&mut s.chat_rows);
                    ensure_streaming_assistant(&mut s.chat_rows);
                }
                AssistantMessageEvent::TextDelta { delta, .. } => {
                    finish_open_thinking(&mut s.chat_rows);
                    append_assistant_delta(&mut s.chat_rows, delta);
                }
                AssistantMessageEvent::TextEnd { .. } => {
                    close_streaming_assistant(&mut s.chat_rows);
                }
                AssistantMessageEvent::ThinkingStart { .. } => {
                    close_streaming_assistant(&mut s.chat_rows);
                    open_thinking(&mut s.chat_rows);
                }
                AssistantMessageEvent::ThinkingDelta { delta, .. } => {
                    append_thinking_delta(&mut s.chat_rows, delta);
                }
                AssistantMessageEvent::ThinkingEnd { .. } => {
                    finish_open_thinking(&mut s.chat_rows);
                }
                AssistantMessageEvent::ToolcallStart { .. }
                | AssistantMessageEvent::ToolcallDelta { .. } => {
                    finish_open_thinking(&mut s.chat_rows);
                    close_streaming_assistant(&mut s.chat_rows);
                    upsert_streaming_tool_calls(&mut s.chat_rows, message);
                }
                AssistantMessageEvent::ToolcallEnd { tool_call, .. } => {
                    finish_open_thinking(&mut s.chat_rows);
                    close_streaming_assistant(&mut s.chat_rows);
                    upsert_tool_row(
                        &mut s.chat_rows,
                        &tool_call.id,
                        &tool_call.name,
                        crate::chat_ui::tool_args_summary(&tool_call.name, &tool_call.arguments),
                        crate::chat_ui::tool_content_preview(&tool_call.name, &tool_call.arguments),
                        ToolCardStatus::Pending,
                    );
                }
                _ => {}
            }
            s.stats.apply_message(message);
        }
        AgentEvent::MessageEnd { message } => {
            finish_open_thinking(&mut s.chat_rows);
            close_streaming_assistant(&mut s.chat_rows);
            s.stats.apply_message(message);
        }
        AgentEvent::ToolExecutionStart {
            tool_call_id,
            tool_name,
            args,
        } => {
            if tool_name == "write" || tool_name == "edit" {
                if let Some(rel) = path_from_tool_args(args) {
                    let path = resolve_project_path(&s.cwd, &rel);
                    let before = std::fs::read_to_string(&path).ok();
                    let after_hint = if tool_name == "write" {
                        args.get("content")
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                    } else {
                        None
                    };
                    file_snapshots.lock().insert(
                        tool_call_id.clone(),
                        FileEditSnapshot {
                            path,
                            before,
                            after_hint,
                        },
                    );
                }
            }
            upsert_tool_row(
                &mut s.chat_rows,
                tool_call_id,
                tool_name,
                crate::chat_ui::tool_args_summary(tool_name, args),
                crate::chat_ui::tool_content_preview(tool_name, args),
                ToolCardStatus::Running,
            );
        }
        AgentEvent::ToolExecutionUpdate {
            tool_call_id,
            tool_name,
            partial_result,
            ..
        } => {
            if !matches!(tool_name.as_str(), "bash" | "shell") {
                return;
            }
            let out = tool_result_text(partial_result);
            if out.is_empty() {
                return;
            }
            if let Some(row) = s
                .chat_rows
                .iter_mut()
                .rev()
                .find(|r| matches!(r, ChatRow::Tool { id, .. } if id == tool_call_id))
            {
                if let ChatRow::Tool { detail, status, .. } = row {
                    // Ignore late updates that race past ToolExecutionEnd.
                    if matches!(*status, ToolCardStatus::Running | ToolCardStatus::Pending)
                        && out.len() >= detail.len()
                    {
                        *detail = out;
                        *status = ToolCardStatus::Running;
                    }
                }
            }
        }
        AgentEvent::ToolExecutionEnd {
            tool_call_id,
            tool_name,
            result,
            is_error,
            ..
        } => {
            if let Some(row) = s
                .chat_rows
                .iter_mut()
                .rev()
                .find(|r| matches!(r, ChatRow::Tool { id, .. } if id == tool_call_id))
            {
                if let ChatRow::Tool { status, detail, .. } = row {
                    *status = if *is_error {
                        ToolCardStatus::Error
                    } else {
                        ToolCardStatus::Success
                    };
                    // Prefer command output for shell; drop write/edit body once FileChange card lands.
                    if matches!(tool_name.as_str(), "bash" | "shell") {
                        let out = tool_result_text(result);
                        if !out.is_empty() {
                            *detail = out;
                        }
                    } else if crate::chat_ui::is_file_mutation_tool(tool_name) && !*is_error {
                        detail.clear();
                    }
                }
            }
            if (tool_name == "write" || tool_name == "edit") && !*is_error {
                let snapshot = file_snapshots.lock().remove(tool_call_id);
                if let Some(change) = pending_change_from_write(result, snapshot, &s.cwd) {
                    s.chat_rows.push(ChatRow::FileChange {
                        id: change.id.clone(),
                        path: change.path.display().to_string(),
                        added: change.added,
                        removed: change.removed,
                    });
                    if s.selected_change_id.is_none() {
                        s.selected_change_id = Some(change.id.clone());
                    }
                    s.pending_changes.push(change);
                }
            }
        }
        _ => {}
    }
}

fn close_streaming_assistant(rows: &mut Vec<ChatRow>) {
    if let Some(ChatRow::Assistant { streaming, .. }) = rows.iter_mut().rev().find(|r| {
        matches!(
            r,
            ChatRow::Assistant {
                streaming: true,
                ..
            }
        )
    }) {
        *streaming = false;
    }
}

fn finish_open_thinking(rows: &mut Vec<ChatRow>) {
    if let Some(ChatRow::Thinking { done, .. }) = rows
        .iter_mut()
        .rev()
        .find(|r| matches!(r, ChatRow::Thinking { done: false, .. }))
    {
        *done = true;
    }
}

fn open_thinking(rows: &mut Vec<ChatRow>) {
    if rows
        .iter()
        .rev()
        .any(|r| matches!(r, ChatRow::Thinking { done: false, .. }))
    {
        return;
    }
    rows.push(ChatRow::Thinking {
        id: uuid::Uuid::now_v7().to_string(),
        text: String::new(),
        done: false,
    });
}

fn append_thinking_delta(rows: &mut Vec<ChatRow>, delta: &str) {
    if let Some(ChatRow::Thinking {
        text, done: false, ..
    }) = rows
        .iter_mut()
        .rev()
        .find(|r| matches!(r, ChatRow::Thinking { done: false, .. }))
    {
        text.push_str(delta);
        return;
    }
    rows.push(ChatRow::Thinking {
        id: uuid::Uuid::now_v7().to_string(),
        text: delta.to_string(),
        done: false,
    });
}

fn ensure_streaming_assistant(rows: &mut Vec<ChatRow>) {
    if rows.iter().rev().any(|r| {
        matches!(
            r,
            ChatRow::Assistant {
                streaming: true,
                ..
            }
        )
    }) {
        return;
    }
    rows.push(ChatRow::Assistant {
        id: uuid::Uuid::now_v7().to_string(),
        text: String::new(),
        streaming: true,
    });
}

fn append_assistant_delta(rows: &mut Vec<ChatRow>, delta: &str) {
    if let Some(ChatRow::Assistant {
        text,
        streaming: true,
        ..
    }) = rows.iter_mut().rev().find(|r| {
        matches!(
            r,
            ChatRow::Assistant {
                streaming: true,
                ..
            }
        )
    }) {
        text.push_str(delta);
        return;
    }
    rows.push(ChatRow::Assistant {
        id: uuid::Uuid::now_v7().to_string(),
        text: delta.to_string(),
        streaming: true,
    });
}

fn upsert_streaming_tool_calls(rows: &mut Vec<ChatRow>, message: &AgentMessage) {
    let AgentMessage::Llm(loop_ai::Message::Assistant(a)) = message else {
        return;
    };
    for block in &a.content {
        let loop_ai::AssistantContent::ToolCall(tc) = block else {
            continue;
        };
        if tc.id.is_empty() || tc.name.is_empty() {
            continue;
        }
        // Show write/edit while args stream so large file bodies don't look hung.
        if !crate::chat_ui::is_file_mutation_tool(&tc.name) {
            continue;
        }
        upsert_tool_row(
            rows,
            &tc.id,
            &tc.name,
            crate::chat_ui::tool_args_summary(&tc.name, &tc.arguments),
            crate::chat_ui::tool_content_preview(&tc.name, &tc.arguments),
            ToolCardStatus::Pending,
        );
    }
}

fn upsert_tool_row(
    rows: &mut Vec<ChatRow>,
    id: &str,
    name: &str,
    summary: String,
    detail: String,
    status: ToolCardStatus,
) {
    if let Some(row) = rows
        .iter_mut()
        .rev()
        .find(|r| matches!(r, ChatRow::Tool { id: tid, .. } if tid == id))
    {
        if let ChatRow::Tool {
            name: existing_name,
            summary: existing_summary,
            detail: existing_detail,
            status: existing_status,
            ..
        } = row
        {
            *existing_name = name.to_string();
            *existing_summary = summary;
            if detail.len() >= existing_detail.len() || existing_detail.is_empty() {
                *existing_detail = detail;
            }
            *existing_status = status;
        }
        return;
    }
    rows.push(ChatRow::Tool {
        id: id.to_string(),
        name: name.to_string(),
        summary,
        detail,
        status,
    });
}

fn path_from_tool_args(args: &Value) -> Option<PathBuf> {
    for key in ["path", "file_path", "file"] {
        if let Some(path) = args.get(key).and_then(|v| v.as_str()) {
            return Some(PathBuf::from(path));
        }
    }
    None
}

fn resolve_project_path(cwd: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    }
}

/// Build a pending change from write/edit tool result + pre-start snapshot.
///
/// Prefer absolute `details.path` / `details.previousPath` from the tool. Fall back to the
/// start-of-tool snapshot (resolved against project cwd) because approval often deletes
/// `previousPath` before `ToolExecutionEnd` is observed.
fn pending_change_from_write(
    result: &AgentToolResult,
    snapshot: Option<FileEditSnapshot>,
    cwd: &Path,
) -> Option<PendingFileChange> {
    let details = &result.details;
    let path = details
        .get("path")
        .and_then(|v| v.as_str())
        .map(PathBuf::from)
        .or_else(|| snapshot.as_ref().map(|s| s.path.clone()))
        .or_else(|| {
            // Last resort: "Wrote N bytes to /abs/path"
            let text = tool_result_text(result);
            text.split_whitespace()
                .rev()
                .find(|tok| looks_like_path(tok))
                .map(PathBuf::from)
        })?;
    let path = resolve_project_path(cwd, &path);

    let before = snapshot
        .as_ref()
        .and_then(|s| s.before.clone())
        .or_else(|| {
            details
                .get("previousPath")
                .and_then(|v| v.as_str())
                .and_then(|p| std::fs::read_to_string(p).ok())
        })
        .or_else(|| {
            if details.get("created").and_then(|v| v.as_bool()) == Some(true) {
                Some(String::new())
            } else {
                None
            }
        });

    let after = std::fs::read_to_string(&path)
        .ok()
        .or_else(|| snapshot.and_then(|s| s.after_hint))
        .unwrap_or_default();

    Some(PendingFileChange::from_paths(path, before, after))
}

fn looks_like_path(s: &str) -> bool {
    let p = Path::new(s);
    p.is_absolute() || p.extension().is_some() || s.contains('/') || s.contains('\\')
}

fn model_display(runtime: &Runtime) -> String {
    runtime
        .selected_model_spec()
        .unwrap_or_else(|| "no model selected".into())
}

fn tool_result_text(result: &AgentToolResult) -> String {
    result
        .content
        .iter()
        .filter_map(|block| match block {
            loop_ai::ToolResultContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

fn list_models(runtime: &Runtime) -> Vec<(String, String, String)> {
    let all = runtime.models.get_models(None);
    let enabled = &runtime.settings.enabled_models;
    let models: Vec<_> = if enabled.is_empty() {
        all
    } else {
        all.into_iter()
            .filter(|m| {
                let key = format!("{}/{}", m.provider, m.id);
                enabled.iter().any(|e| e == &key || e == &m.id)
            })
            .collect()
    };
    models
        .into_iter()
        .map(|m| (m.provider.clone(), m.id.clone(), m.name.clone()))
        .collect()
}

fn thinking_label_str(level: AgentThinkingLevel) -> String {
    match level {
        AgentThinkingLevel::Off => "off".into(),
        AgentThinkingLevel::Minimal => "minimal".into(),
        AgentThinkingLevel::Low => "low".into(),
        AgentThinkingLevel::Medium => "medium".into(),
        AgentThinkingLevel::High => "high".into(),
        AgentThinkingLevel::XHigh => "xhigh".into(),
        AgentThinkingLevel::Max => "max".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use loop_ai::{AssistantContent, AssistantMessage, StopReason, TextContent, ToolCall, Usage};
    use serde_json::json;

    fn assistant_with_write(text: &str, path: &str, content: &str) -> AgentMessage {
        AgentMessage::assistant(AssistantMessage {
            content: vec![
                AssistantContent::Text(TextContent {
                    text: text.to_string(),
                    text_signature: None,
                }),
                AssistantContent::ToolCall(ToolCall {
                    id: "call_write".into(),
                    name: "write".into(),
                    arguments: json!({ "path": path, "content": content }),
                    thought_signature: None,
                }),
            ],
            api: "test".into(),
            provider: "test".into(),
            model: "test".into(),
            response_model: None,
            response_id: None,
            usage: Usage::empty(),
            stop_reason: StopReason::Pending,
            error_message: None,
            raw_stop_reason: None,
            timestamp: 0,
        })
    }

    #[test]
    fn thinking_then_text_streams_via_deltas() {
        let mut rows = Vec::new();
        open_thinking(&mut rows);
        append_thinking_delta(&mut rows, "plan");
        assert!(matches!(
            rows.as_slice(),
            [ChatRow::Thinking {
                text,
                done: false,
                ..
            }] if text == "plan"
        ));

        finish_open_thinking(&mut rows);
        append_assistant_delta(&mut rows, "Hello");
        assert!(matches!(
            rows.as_slice(),
            [
                ChatRow::Thinking { done: true, .. },
                ChatRow::Assistant {
                    text,
                    streaming: true,
                    ..
                }
            ] if text == "Hello"
        ));

        append_assistant_delta(&mut rows, " world");
        assert!(matches!(
            rows.as_slice(),
            [
                ChatRow::Thinking { done: true, .. },
                ChatRow::Assistant {
                    text,
                    streaming: true,
                    ..
                }
            ] if text == "Hello world"
        ));
    }

    #[test]
    fn toolcall_stream_does_not_duplicate_assistant_text() {
        let mut rows = Vec::new();
        let intro = "I'll create a Python script.";
        append_assistant_delta(&mut rows, intro);
        close_streaming_assistant(&mut rows);

        // Simulate several tool-call deltas that still carry the intro text in `message`.
        for body in ["p", "print(1)", "print(1)\n"] {
            let msg = assistant_with_write(intro, "sort.py", body);
            finish_open_thinking(&mut rows);
            close_streaming_assistant(&mut rows);
            upsert_streaming_tool_calls(&mut rows, &msg);
        }

        let assistant_count = rows
            .iter()
            .filter(|r| matches!(r, ChatRow::Assistant { .. }))
            .count();
        assert_eq!(assistant_count, 1, "rows={rows:?}");
        assert!(matches!(
            rows.as_slice(),
            [
                ChatRow::Assistant {
                    text,
                    streaming: false,
                    ..
                },
                ChatRow::Tool {
                    name,
                    summary,
                    detail,
                    status: ToolCardStatus::Pending,
                    ..
                }
            ] if text == intro
                && name == "write"
                && summary == "sort.py"
                && detail == "print(1)\n"
        ));
    }

    #[test]
    fn text_after_tool_starts_new_bubble_not_clone() {
        let mut rows = Vec::new();
        append_assistant_delta(&mut rows, "Intro.");
        close_streaming_assistant(&mut rows);
        upsert_streaming_tool_calls(&mut rows, &assistant_with_write("Intro.", "a.py", "x"));
        close_streaming_assistant(&mut rows);

        // Later model text (after tools) must be a new bubble.
        append_assistant_delta(&mut rows, "Done.");
        assert!(matches!(
            rows.as_slice(),
            [
                ChatRow::Assistant { text: a, streaming: false, .. },
                ChatRow::Tool { .. },
                ChatRow::Assistant { text: b, streaming: true, .. },
            ] if a == "Intro." && b == "Done."
        ));
    }

    #[test]
    fn pending_change_uses_details_path_and_after_hint() {
        let dir = std::env::temp_dir().join(format!("loop-desktop-diff-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&dir).unwrap();
        let abs = dir.join("sort_script.py");
        let after = "print('sorted')\n";
        std::fs::write(&abs, after).unwrap();

        let result = AgentToolResult {
            content: vec![loop_ai::ToolResultContent::Text(TextContent {
                text: format!("Wrote {} bytes to {}", after.len(), abs.display()),
                text_signature: None,
            })],
            details: json!({
                "path": abs,
                "bytes": after.len(),
                "created": true,
                // Simulate approval having already deleted previousPath.
                "previousPath": dir.join("missing.before"),
            }),
            usage: None,
            added_tool_names: None,
            terminate: None,
        };

        // Snapshot used a relative path previously; after_hint recovers content if needed.
        let change = pending_change_from_write(
            &result,
            Some(FileEditSnapshot {
                path: PathBuf::from("sort_script.py"),
                before: None,
                after_hint: Some(after.to_string()),
            }),
            &dir,
        )
        .expect("change");

        let _ = std::fs::remove_dir_all(&dir);

        assert!(change.added > 0, "added={}", change.added);
        assert_eq!(change.removed, 0);
        assert_eq!(change.after, after);
        assert_eq!(change.before.as_deref(), Some(""));
    }
}
