//! Interactive inline CLI (Pi-style): transcript in scrollback, footer redrawn in place.

use std::collections::VecDeque;
use std::io::{self, Stdout};
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use crossterm::event::{
    self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, KeyboardEnhancementFlags, PopKeyboardEnhancementFlags,
    PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use ratatui::backend::CrosstermBackend;
use ratatui::{Terminal, TerminalOptions, Viewport};
use tokio::sync::mpsc;

use loop_agent::harness::{
    format_prompt_template_invocation, KrunSandbox, Sandbox, SandboxMode, SessionForkPoint,
    SessionForkSelection,
};
use loop_agent::types::AgentThinkingLevel;
use loop_ai::providers::SOKET_BASE_URL;
use loop_ai::{calculate_context_tokens, ModelsRefreshOptions, Usage};
use loop_app_core::interaction::{
    approval_kind, ApprovalReply, InteractionQueue, PendingInteraction,
};
use loop_protocol::{
    ContentBlock, InteractionOutcome, InteractionRequest, NoticeLevel, Origin, Role, SessionSpec,
    SessionUpdate, StopReason, ToolCallStatus, ToolContent, TurnOutcome, UpdateEnvelope,
};

#[cfg(feature = "telemetry")]
use crate::commands::TracingCommand;
use crate::commands::{self, AutocompleteEntry, CommandEffect, McpCommand, SkillsCommand};
#[cfg(feature = "telemetry")]
use crate::config::describe_tracing_status;
use crate::keybindings::{
    hotkey_help, multiline_paste_text, normalize_pasted_text, paste_burst_pending,
    pasted_single_line, Action,
};
use crate::provider_setup::ProviderSetup;
use crate::setup_prompt::SetupPrompt;
use crate::theme::Theme;
use crate::tool_approval::{
    auto_approve_from_entries, permissions_from_settings, ApprovalDecision, ApprovalKind,
    ApprovalPolicy, ToolApprovalBridge, GROUP_BASH, GROUP_FILE,
};
#[cfg(feature = "telemetry")]
use crate::tracing_setup::TracingSetup;
#[cfg(feature = "orchestration")]
use crate::tui::find_workflow_task_index;
use crate::tui::{
    consume_frozen_lines, filter_files, find_at_mention, find_tool_index, footer_live_height,
    format_item_lines, format_live_lines, format_token_usage_line, insert_text, item_is_committed,
    list_files, live_overflow_count, render_lines_to_buffer, tool_args_summary, welcome_lines,
    CardStatus, ChatItem, CommandHistory, FileEntry, FooterOpts, InputBuffer, McpServerListing,
    PickerRow, PickerView, FOOTER_HEIGHT,
};
use crate::{build_tools, CliRuntime};

enum UiEvent {
    UpdateAvailable(String),
    /// An update from the agent backend.
    Update(Box<UpdateEnvelope>),
    /// The backend refused to start a turn (no update was emitted for it).
    TurnFailed(String),
    /// `/compact` finished (success message or error).
    CompactDone(Result<String, String>),
    /// `/sandbox` enable/disable finished.
    SandboxDone(Result<SandboxDoneOk, String>),
    /// Multi-agent workflow progress update.
    #[cfg(feature = "orchestration")]
    WorkflowDone(Result<WorkflowDoneOk, String>),
    /// Planned task graph ready to display.
    #[cfg(feature = "orchestration")]
    WorkflowGraph {
        outline: String,
        mermaid: String,
    },
    /// Workflow task started executing.
    #[cfg(feature = "orchestration")]
    WorkflowTaskStarted {
        task_id: String,
        description: String,
    },
    /// Workflow task completed.
    #[cfg(feature = "orchestration")]
    WorkflowTaskCompleted {
        task_id: String,
        output: String,
    },
    /// Workflow task failed.
    #[cfg(feature = "orchestration")]
    WorkflowTaskFailed {
        task_id: String,
        error: String,
    },
    /// `/mcp login` has an authorization URL ready to show.
    McpAuthUrl {
        name: String,
        url: String,
        browser_error: Option<String>,
    },
    /// `/mcp login` finished (tokens saved, or an error).
    McpLoginDone {
        name: String,
        result: Result<loop_mcp::OAuthLoginOutcome, String>,
    },
}

/// Successful workflow completion info.
#[cfg(feature = "orchestration")]
struct WorkflowDoneOk {
    success: bool,
    completed_count: usize,
    failed_count: usize,
    total_count: usize,
    output: String,
    artifacts: Vec<String>,
}

/// Successful sandbox switch applied on the UI thread.
enum SandboxDoneOk {
    /// Sandbox disabled.
    Off,
    /// Local sandbox enabled.
    Local { isolation: String, runtime: String },
}

/// Pending accept/reject prompt for a tool call.
struct ActiveApproval {
    kind: ApprovalKind,
    tool_name: String,
    summary: String,
    selected: usize,
    reason: String,
    reason_focused: bool,
    reply: ApprovalReply,
    /// The interaction this picker answers, to close it if withdrawn.
    interaction: loop_protocol::InteractionId,
}

impl ActiveApproval {
    /// The approval picker for a permission request. Other interaction kinds
    /// are answered right away: the terminal cannot present them yet.
    fn from_interaction(pending: PendingInteraction) -> Option<Self> {
        let PendingInteraction { request, respond } = pending;
        let InteractionRequest::Permission {
            id,
            tool_call,
            options,
        } = request
        else {
            let _ = respond.send(InteractionOutcome::Rejected {
                reason: "this request type is not supported in the terminal yet".into(),
            });
            return None;
        };
        let kind = approval_kind(tool_call.as_ref().and_then(|c| c.kind));
        let tool_name = tool_call
            .as_ref()
            .and_then(|c| c.name.clone())
            .unwrap_or_else(|| kind.label().into());
        let summary = tool_call
            .as_ref()
            .and_then(|c| c.title.clone())
            .unwrap_or_default();
        Some(Self {
            kind,
            tool_name,
            summary,
            selected: 0,
            reason: String::new(),
            reason_focused: false,
            reply: ApprovalReply::new(&options, respond),
            interaction: id,
        })
    }

    fn to_picker(&self) -> PickerView {
        PickerView::FileReview {
            path: format!("{} · {}", self.tool_name, self.summary),
            selected: self.selected,
            accept_label: self.reply.accept_label(),
            accept_all_label: self.reply.accept_session_label(),
            reason: self.reason.clone(),
            reason_focused: self.reason_focused,
        }
    }
}

/// Outbound user message waiting for the agent to become idle.
#[derive(Debug, Clone)]
struct QueuedMessage {
    /// Shown in the transcript (and matched to `ChatItem::Queued`).
    display: String,
    /// Sent to `harness.prompt`.
    prompt: String,
}

/// Live token / context stats for the footer usage line.
#[derive(Debug, Clone)]
struct TokenBarState {
    total_tokens: u64,
    context_tokens: Option<u64>,
    context_window: u64,
}

impl TokenBarState {
    fn from_model(runtime: &CliRuntime) -> Self {
        let context_window = runtime
            .selected_model
            .as_ref()
            .map_or(0, |m| m.context_window);
        Self {
            total_tokens: 0,
            context_tokens: Some(0),
            context_window,
        }
    }

    async fn load(runtime: &CliRuntime) -> Self {
        let mut state = Self::from_model(runtime);
        state.refresh(runtime).await;
        state
    }

    async fn refresh(&mut self, runtime: &CliRuntime) {
        self.sync_window(runtime);
        let Some(service) = runtime.backend.session_stats() else {
            return;
        };
        if let Ok(stats) = service.stats(&runtime.session_ref()).await {
            self.total_tokens = stats.total_tokens;
            if let Some(window) = stats.context_window {
                self.context_tokens = stats.context_tokens;
                self.context_window = window;
            }
        }
    }

    fn sync_window(&mut self, runtime: &CliRuntime) {
        if let Some(m) = &runtime.selected_model {
            self.context_window = m.context_window;
        }
    }

    fn reset(&mut self, runtime: &CliRuntime) {
        *self = Self::from_model(runtime);
    }

    fn apply_usage(&mut self, usage: &Usage) {
        self.total_tokens = self
            .total_tokens
            .saturating_add(usage.input)
            .saturating_add(usage.output)
            .saturating_add(usage.cache_read)
            .saturating_add(usage.cache_write);
        let ctx = calculate_context_tokens(usage);
        if ctx > 0 {
            self.context_tokens = Some(ctx);
        }
    }

    fn apply_protocol_usage(&mut self, usage: &loop_protocol::Usage) {
        self.apply_usage(&Usage {
            input: usage.input_tokens,
            output: usage.output_tokens,
            cache_read: usage.cached_input_tokens.unwrap_or(0),
            cache_write: usage.cache_write_tokens.unwrap_or(0),
            total_tokens: usage.total_tokens,
            ..Usage::default()
        });
    }

    fn usage_line(&self) -> String {
        format_token_usage_line(self.total_tokens, self.context_tokens, self.context_window)
    }
}

/// Run the interactive CLI (inline viewport — native terminal scrollback).
pub async fn run(mut runtime: CliRuntime) -> anyhow::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    let _ = execute!(
        stdout,
        EnableBracketedPaste,
        PushKeyboardEnhancementFlags(
            KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES
                | KeyboardEnhancementFlags::REPORT_ALTERNATE_KEYS
        )
    );
    // Start on a clean screen: clear and home before anchoring the viewport.
    let _ = execute!(
        stdout,
        crossterm::terminal::Clear(crossterm::terminal::ClearType::All),
        crossterm::cursor::MoveTo(0, 0)
    );
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(FOOTER_HEIGHT),
        },
    )?;
    let _ = terminal.hide_cursor();

    let result = run_loop(&mut terminal, &mut runtime).await;

    disable_raw_mode()?;
    let _ = execute!(
        terminal.backend_mut(),
        DisableBracketedPaste,
        PopKeyboardEnhancementFlags
    );
    // Clear the inline footer so the shell prompt lands cleanly.
    let _ = terminal.clear();
    terminal.show_cursor()?;

    match result {
        Ok(Some(session_id)) => {
            println!("You can resume this session with: loop --resume {session_id}");
            Ok(())
        }
        Ok(None) => Ok(()),
        Err(e) => Err(e),
    }
}

async fn run_loop(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    runtime: &mut CliRuntime,
) -> anyhow::Result<Option<String>> {
    let (tx, mut rx) = mpsc::unbounded_channel::<UiEvent>();
    // A best-effort notification. Never replace files or block startup.
    if loop_update::Installation::is_managed()
        && !loop_update::update_check_disabled(
            std::env::var("LOOP_NO_UPDATE_CHECK").ok().as_deref(),
        )
    {
        let update_tx = tx.clone();
        tokio::spawn(async move {
            let result = async {
                let client = loop_update::ReleaseClient::github(Duration::from_secs(5))?;
                let cache = loop_update::Installation::notification_cache_path()?;
                let release = client.latest_cached(&cache).await?;
                if loop_update::is_newer(env!("CARGO_PKG_VERSION"), &release)? {
                    let _ = update_tx.send(UiEvent::UpdateAvailable(format!(
                        "Loop {} is available (installed: {}). Exit this session and run `loop update` to install it.",
                        release.version, env!("CARGO_PKG_VERSION")
                    )));
                }
                Ok::<_, loop_update::Error>(())
            }.await;
            if let Err(error) = result {
                tracing::debug!(%error, "Update check skipped");
            }
        });
    }
    forward_updates(
        runtime.backend.subscribe(),
        tx.clone(),
        runtime.debug_log_path.clone(),
    );

    let review_policy = ApprovalPolicy::parse(&runtime.settings.file_edit_review);
    let policy_active = review_policy.asks_for_session(!runtime.resumed);
    let tool_env = runtime.harness.tool_env().await?;
    let session_grants = match runtime.harness.read_session_entries().await {
        Ok(entries) => auto_approve_from_entries(&entries),
        Err(e) => {
            tracing::warn!("failed to load session tool approvals: {e}");
            Default::default()
        }
    };
    let (host, mut review_rx) = InteractionQueue::new();
    let bridge = Arc::new(ToolApprovalBridge::with_host(
        tool_env,
        runtime.settings.diff_editor.clone(),
        policy_active,
        permissions_from_settings(&runtime.settings.tool_permissions),
        session_grants,
        Arc::new(host),
    ));
    {
        let harness = Arc::clone(&runtime.harness);
        bridge.set_persist(Arc::new(move |label| {
            let harness = Arc::clone(&harness);
            Box::pin(async move {
                if let Err(e) = harness.append_label(label).await {
                    tracing::warn!("persist tool approval label: {e}");
                }
            })
        }));
    }
    runtime
        .harness
        .set_before_tool_call(Some(bridge.before_tool_hook()));
    runtime
        .harness
        .set_after_tool_call(Some(bridge.after_tool_hook()));
    runtime.tool_approval = Some(Arc::clone(&bridge));

    let mut chat: Vec<ChatItem> = match runtime.backend.transcript(&runtime.session_ref()).await {
        Ok(updates) => chat_from_transcript(updates),
        Err(e) => {
            tracing::warn!("failed to restore session transcript: {e}");
            Vec::new()
        }
    };
    let mut flushed = 0usize;
    // Visual lines of `chat[flushed..]` already pushed into native scrollback.
    let mut live_frozen = 0usize;
    let mut input = InputBuffer::new();
    let mut history = CommandHistory::load(crate::config::paths::history_path(&runtime.agent_dir));
    let mut status: String = if runtime.needs_provider_setup {
        "login · connect a model provider to begin".into()
    } else if runtime.selected_model.is_none() {
        NO_MODEL_LABEL.into()
    } else if runtime.resumed {
        if chat.is_empty() {
            "resumed · empty session · /help for commands".into()
        } else {
            format!("resumed · {} messages · /help for commands", chat.len())
        }
    } else {
        let review_hint = if policy_active {
            " · tool approval on (files + bash)"
        } else {
            ""
        };
        format!("ready · /help for commands{review_hint}")
    };
    let mut clear_presses = 0u8;
    let mut last_clear = Instant::now();
    let mut last_escape = Instant::now();
    let mut streaming_assistant: Option<usize> = None;
    let mut streaming_thinking: Option<usize> = None;
    // Pi-style: one global expand flag; toggling clears + reprints the transcript.
    let mut expand_details = false;
    let mut redraw_request = false;
    let mut purge_ui_events = false;
    let mut last_width = terminal.size()?.width;
    let mut hide_thinking = runtime.settings.hide_thinking_block;
    let mut pending_setup: Option<SetupPrompt> = if runtime.needs_provider_setup {
        ProviderSetup::start(None).ok().map(SetupPrompt::Provider)
    } else {
        None
    };
    let mut model_picker: Option<ModelPickerState> = None;
    let mut fork_picker: Option<ForkPickerState> = None;
    let mut skill_picker: Option<SkillPickerState> = None;
    let mut mcp_picker: Option<McpPickerState> = None;
    let mut active_approval: Option<ActiveApproval> = None;
    let mut ac_selected: usize = 0;
    let mut last_ac_filter = String::new();
    let mut file_index: Option<Vec<FileEntry>> = None;
    let mut working = false;
    let mut turn_step = String::new();
    let mut message_queue: VecDeque<QueuedMessage> = VecDeque::new();
    let mut spinner_frame: usize = 0;
    let mut last_spin = Instant::now();
    let version = env!("CARGO_PKG_VERSION");
    let path_line = path_status_line(&runtime.cwd);
    let mut token_bar = TokenBarState::load(runtime).await;
    let mut refresh_token_bar = false;

    // Welcome banner into terminal scrollback (above the footer).
    print_welcome(terminal, runtime, version)?;
    if let Some(path) = &runtime.debug_log_path {
        let msg = format!("debug log → {}", path.display());
        terminal.insert_before(1, |buf| {
            let line = ratatui::text::Line::from(ratatui::text::Span::styled(
                format!(" {msg}"),
                runtime.theme.muted(),
            ));
            render_lines_to_buffer(&[line], buf, &runtime.theme);
        })?;
    }

    let tick = Duration::from_millis(33);
    let mut should_quit = false;

    while !should_quit {
        if working && last_spin.elapsed() >= Duration::from_millis(80) {
            spinner_frame = spinner_frame.wrapping_add(1);
            last_spin = Instant::now();
        }

        // Flush finished transcript items into native scrollback.
        if flushed > chat.len() {
            flushed = chat.len();
            live_frozen = 0;
        }
        // Mid-stream spill freezes lines rendered from incomplete markdown
        // (tables especially). Once the item commits, reprint the transcript
        // so those prefix lines are replaced with a final full-width layout.
        if flush_committed(
            terminal,
            &chat,
            &mut flushed,
            &mut live_frozen,
            streaming_assistant,
            streaming_thinking,
            &runtime.theme,
            expand_details,
            hide_thinking,
        )? {
            redraw_request = true;
        }

        let model_line = match runtime.selected_model_spec() {
            Some(spec) => format!("{spec} · {}", runtime.settings.default_thinking_level),
            None => NO_MODEL_LABEL.to_string(),
        };

        let ac_entries = if input.as_str().starts_with('/')
            && !input.as_str().contains(' ')
            && pending_setup.is_none()
            && model_picker.is_none()
            && fork_picker.is_none()
            && skill_picker.is_none()
            && mcp_picker.is_none()
            && active_approval.is_none()
        {
            let extra = dynamic_command_entries(runtime);
            commands::autocomplete_entries(input.as_str(), &extra)
        } else {
            Vec::new()
        };
        let at_mention = if ac_entries.is_empty()
            && pending_setup.is_none()
            && model_picker.is_none()
            && fork_picker.is_none()
            && skill_picker.is_none()
            && mcp_picker.is_none()
            && active_approval.is_none()
        {
            find_at_mention(input.as_str(), input.cursor())
        } else {
            None
        };
        let file_ac_entries = if let Some(ref mention) = at_mention {
            let index = file_index.get_or_insert_with(|| list_files(&runtime.cwd));
            filter_files(index, &mention.query, 100)
        } else {
            Vec::new()
        };
        let ac_filter_key = if let Some(ref m) = at_mention {
            format!("@{}", m.query)
        } else {
            input.as_str().to_string()
        };
        if ac_filter_key != last_ac_filter {
            ac_selected = 0;
            last_ac_filter = ac_filter_key;
        }
        let picker_len = if !ac_entries.is_empty() {
            ac_entries.len()
        } else {
            file_ac_entries.len()
        };
        if picker_len > 0 {
            ac_selected = ac_selected.min(picker_len - 1);
        }

        let picker = if let Some(review) = &active_approval {
            review.to_picker()
        } else if let Some(p) = &model_picker {
            let current = runtime.selected_model_spec().unwrap_or_default();
            PickerView::Models {
                rows: p
                    .filtered
                    .iter()
                    .map(|id| {
                        let (label, desc) = match id.split_once('/') {
                            Some((prov, model)) => (model.to_string(), format!("[{prov}]")),
                            None => (id.clone(), String::new()),
                        };
                        PickerRow {
                            label,
                            description: desc,
                            mark: if *id == current {
                                Some("✓".into())
                            } else {
                                None
                            },
                        }
                    })
                    .collect(),
                selected: p.selected,
                hint: "Only showing models from configured providers. Use /login to add providers."
                    .into(),
            }
        } else if let Some(p) = &fork_picker {
            PickerView::Models {
                rows: p
                    .filtered
                    .iter()
                    .filter_map(|&i| p.points.get(i))
                    .map(|pt| PickerRow {
                        label: format!("#{}", pt.index),
                        description: pt.preview.clone(),
                        mark: None,
                    })
                    .collect(),
                selected: p.selected,
                hint: "Fork: edit this user message and continue (prior history kept).".into(),
            }
        } else if let Some(p) = &skill_picker {
            PickerView::Models {
                rows: p
                    .filtered
                    .iter()
                    .filter_map(|&i| p.skills.get(i))
                    .map(|(name, desc)| {
                        let enabled = runtime.resources.skills.iter().any(|s| &s.name == name);
                        PickerRow {
                            label: format!("[{}] {name}", if enabled { "x" } else { " " }),
                            description: desc.clone(),
                            mark: None,
                        }
                    })
                    .collect(),
                selected: p.selected,
                hint: "Skills: [x] enabled · [ ] disabled. Changes are saved to settings.json."
                    .into(),
            }
        } else if let Some(p) = &mcp_picker {
            PickerView::Models {
                rows: p
                    .filtered
                    .iter()
                    .filter_map(|&i| p.servers.get(i))
                    .map(|(name, desc)| {
                        let enabled = !runtime
                            .settings
                            .disabled_mcp_servers
                            .iter()
                            .any(|n| n == name);
                        PickerRow {
                            label: format!("[{}] {name}", if enabled { "x" } else { " " }),
                            description: desc.clone(),
                            mark: None,
                        }
                    })
                    .collect(),
                selected: p.selected,
                hint: "MCP: [x] enabled · [ ] disabled. Changes are saved to settings.json.".into(),
            }
        } else if let Some(prompt) = &pending_setup {
            PickerView::Setup {
                prompt: prompt.clone(),
                first_run: runtime.needs_provider_setup,
            }
        } else if !ac_entries.is_empty() {
            PickerView::Commands {
                rows: ac_entries
                    .iter()
                    .map(|e| PickerRow {
                        label: e.name.clone(),
                        description: e.description.clone(),
                        mark: None,
                    })
                    .collect(),
                selected: ac_selected,
            }
        } else if !file_ac_entries.is_empty() {
            PickerView::Commands {
                rows: file_ac_entries
                    .iter()
                    .map(|e| PickerRow {
                        label: e.relative.clone(),
                        description: e.absolute.clone(),
                        mark: None,
                    })
                    .collect(),
                selected: ac_selected,
            }
        } else {
            PickerView::None
        };

        let status_line = if active_approval.is_some() {
            "tool approval · ↑↓ choose · tab reason · enter confirm".into()
        } else if model_picker.is_some() {
            "↑↓ select · enter confirm · esc cancel".into()
        } else if fork_picker.is_some() {
            "↑↓ select · enter edit · esc cancel".into()
        } else if skill_picker.is_some() || mcp_picker.is_some() {
            "↑↓ select · space/enter toggle · type to filter · esc close".into()
        } else if let Some(prompt) = &pending_setup {
            prompt.status_hint(runtime.needs_provider_setup)
        } else if !ac_entries.is_empty() {
            "↑↓ select · tab complete · enter run".into()
        } else if !file_ac_entries.is_empty() {
            "↑↓ select · tab complete · enter insert".into()
        } else if working {
            let pending_tools = chat
                .iter()
                .filter(|c| {
                    matches!(
                        c,
                        ChatItem::Tool {
                            status: CardStatus::Pending,
                            ..
                        }
                    )
                })
                .count();
            let queued = message_queue.len();
            // Alternate every ~1.2s between the current step and "Working…".
            let show_step = !turn_step.is_empty() && (spinner_frame / 15).is_multiple_of(2);
            let mut parts = vec![if show_step {
                turn_step.clone()
            } else {
                "Working…".to_string()
            }];
            if pending_tools > 0 {
                parts.push(format!("{pending_tools} tool(s)"));
            }
            if queued > 0 {
                parts.push(format!("{queued} queued"));
            }
            parts.join(" · ")
        } else if !message_queue.is_empty() {
            format!("{} queued · sending…", message_queue.len())
        } else if !runtime.active_skills.is_empty() {
            let names = runtime.active_skills.join(", ");
            format!("skill(s) active: {names} · type your message")
        } else {
            status.clone()
        };

        let live: Vec<ChatItem> = chat[flushed..].to_vec();
        let setup_mode = pending_setup.is_some();
        if refresh_token_bar {
            token_bar.refresh(runtime).await;
            refresh_token_bar = false;
        }
        let usage_line = token_bar.usage_line();
        let term_width = terminal.size()?.width;
        let live_h =
            footer_live_height(FOOTER_HEIGHT, term_width, input.as_str(), &picker) as usize;
        let live_lines = format_live_lines(
            &live,
            &runtime.theme,
            expand_details,
            hide_thinking,
            term_width,
            working,
            spinner_frame,
            &status_line,
        );
        spill_live_overflow(
            terminal,
            &live_lines,
            &mut live_frozen,
            live_h,
            &runtime.theme,
        )?;
        let shown_live = {
            let skip = live_frozen.min(live_lines.len());
            &live_lines[skip..]
        };

        terminal.draw(|f| {
            crate::tui::draw_footer(
                f,
                FooterOpts {
                    theme: &runtime.theme,
                    live_lines: shown_live,
                    input: input.as_str(),
                    cursor: input.cursor(),
                    working,
                    spinner_frame,
                    status: &status_line,
                    picker: &picker,
                    setup_mode,
                    mask_input: pending_setup.as_ref().is_some_and(SetupPrompt::masked),
                    setup_placeholder: pending_setup.as_ref().map_or("", SetupPrompt::placeholder),
                    path_line: &path_line,
                    model_line: &model_line,
                    usage_line: &usage_line,
                },
            );
        })?;
        let _ = terminal.hide_cursor();

        let timed_out = !event::poll(tick)?;
        if timed_out {
            drain_ui_events(
                runtime,
                &mut rx,
                &mut review_rx,
                &mut active_approval,
                &mut chat,
                &mut status,
                &mut turn_step,
                &mut streaming_assistant,
                &mut streaming_thinking,
                &mut working,
                &mut message_queue,
                &mut token_bar,
                &mut refresh_token_bar,
                &tx,
            )
            .await;
            continue;
        }

        loop {
            // A multiline paste arrives as one bracketed `Event::Paste`, or — when
            // the terminal does not bracket it — as a burst of keys whose newlines
            // would otherwise each submit. Hold that burst as one composer block.
            let batch = read_input_batch()?;
            if let Some(keys) = unbracketed_paste_keys(&batch) {
                if let Some(text) = multiline_paste_text(&keys) {
                    insert_pasted_text(
                        &text,
                        runtime,
                        &mut input,
                        &pending_setup,
                        &mut model_picker,
                        &mut fork_picker,
                        &mut skill_picker,
                        &mut mcp_picker,
                        &mut active_approval,
                    );
                    for ev in &batch {
                        if let Event::Resize(w, _) = ev {
                            note_terminal_resize(
                                terminal,
                                *w,
                                &mut last_width,
                                &mut redraw_request,
                            );
                        }
                    }
                    token_bar.sync_window(runtime);
                    if should_quit || !event::poll(Duration::from_millis(0))? {
                        break;
                    }
                    continue;
                }
            }
            for ev in batch {
                match ev {
                    Event::Paste(text) => {
                        insert_pasted_text(
                            &text,
                            runtime,
                            &mut input,
                            &pending_setup,
                            &mut model_picker,
                            &mut fork_picker,
                            &mut skill_picker,
                            &mut mcp_picker,
                            &mut active_approval,
                        );
                        token_bar.sync_window(runtime);
                    }
                    Event::Key(key) if key.kind == KeyEventKind::Press => {
                        handle_key(
                            key,
                            runtime,
                            &mut input,
                            &mut history,
                            &mut chat,
                            &mut status,
                            &mut turn_step,
                            &mut clear_presses,
                            &mut last_clear,
                            &mut last_escape,
                            &mut streaming_assistant,
                            &mut streaming_thinking,
                            &mut expand_details,
                            &mut redraw_request,
                            &mut purge_ui_events,
                            &mut hide_thinking,
                            &mut pending_setup,
                            &mut model_picker,
                            &mut fork_picker,
                            &mut skill_picker,
                            &mut mcp_picker,
                            &mut active_approval,
                            &mut working,
                            &mut message_queue,
                            &mut should_quit,
                            &ac_entries,
                            &file_ac_entries,
                            at_mention.as_ref(),
                            &mut ac_selected,
                            &tx,
                            &mut token_bar,
                            &mut refresh_token_bar,
                        )
                        .await?;
                        token_bar.sync_window(runtime);
                    }
                    Event::Resize(w, _) => {
                        note_terminal_resize(terminal, w, &mut last_width, &mut redraw_request);
                    }
                    _ => {}
                }
                if should_quit {
                    break;
                }
            }
            if should_quit || !event::poll(Duration::from_millis(0))? {
                break;
            }
        }

        // Drop events from an aborted turn so they can't repopulate a fresh session.
        if purge_ui_events {
            while rx.try_recv().is_ok() {}
            while review_rx.try_recv().is_ok() {}
            if let Some(r) = active_approval.take() {
                r.reply.send(ApprovalDecision::Reject {
                    reason: Some("session reset".into()),
                });
            }
            purge_ui_events = false;
            working = false;
            turn_step.clear();
            streaming_assistant = None;
            streaming_thinking = None;
        }

        // Pi-style hard reset: clear screen + scrollback, then reprint the whole
        // transcript with the current expand state. Native scrollback handles
        // anything taller than the viewport.
        if redraw_request {
            redraw_request = false;
            reset_and_redraw(
                terminal,
                runtime,
                version,
                &chat,
                &mut flushed,
                &mut live_frozen,
                streaming_assistant,
                streaming_thinking,
                expand_details,
                hide_thinking,
            )?;
        }

        drain_ui_events(
            runtime,
            &mut rx,
            &mut review_rx,
            &mut active_approval,
            &mut chat,
            &mut status,
            &mut turn_step,
            &mut streaming_assistant,
            &mut streaming_thinking,
            &mut working,
            &mut message_queue,
            &mut token_bar,
            &mut refresh_token_bar,
            &tx,
        )
        .await;
    }

    runtime.harness.request_shutdown();
    let has_user_message = chat
        .iter()
        .any(|item| matches!(item, ChatItem::User { .. }));
    Ok(has_user_message.then(|| runtime.session_id.clone()))
}

#[allow(clippy::too_many_arguments)]
/// Returns `true` when the caller should hard-reset and reprint the transcript
/// (spilled live lines were rendered mid-stream and must not be kept).
fn flush_committed(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    chat: &[ChatItem],
    flushed: &mut usize,
    live_frozen: &mut usize,
    streaming_assistant: Option<usize>,
    streaming_thinking: Option<usize>,
    theme: &Theme,
    expanded: bool,
    hide_thinking: bool,
) -> io::Result<bool> {
    let width = terminal.size()?.width;
    while *flushed < chat.len() {
        if !item_is_committed(
            &chat[*flushed],
            *flushed,
            streaming_assistant,
            streaming_thinking,
        ) {
            break;
        }
        // Items land in scrollback with the current global expand state; toggling
        // ctrl+o clears and reprints everything (see `reset_and_redraw`).
        // Prefix lines may already have spilled while the item was streaming —
        // those used incomplete content / different wrap, so skip-and-append
        // would leave a broken table (etc.) in native scrollback.
        let lines = format_item_lines(&chat[*flushed], theme, expanded, hide_thinking, width);
        let (skip, next_frozen) = consume_frozen_lines(*live_frozen, lines.len());
        if skip > 0 {
            return Ok(true);
        }
        if skip < lines.len() {
            insert_scrollback_lines(terminal, &lines[skip..], theme)?;
        }
        *live_frozen = next_frozen;
        *flushed += 1;
    }
    Ok(false)
}

fn insert_scrollback_lines(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    lines: &[ratatui::text::Line<'static>],
    theme: &Theme,
) -> io::Result<()> {
    const CHUNK: usize = 256;
    for chunk in lines.chunks(CHUNK) {
        if chunk.is_empty() {
            continue;
        }
        let h = chunk.len() as u16;
        terminal.insert_before(h, |buf| {
            render_lines_to_buffer(chunk, buf, theme);
        })?;
    }
    Ok(())
}

/// Push live/streaming lines that no longer fit in the footer into native
/// scrollback so the transcript stays continuous and terminal-scrollable.
fn spill_live_overflow(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    live_lines: &[ratatui::text::Line<'static>],
    live_frozen: &mut usize,
    live_h: usize,
    theme: &Theme,
) -> io::Result<()> {
    let overflow = live_overflow_count(live_lines.len(), live_h);
    // Only rewind when the render itself shrank (viewport growth must not
    // re-show lines that are already in scrollback).
    if live_lines.len() < *live_frozen {
        *live_frozen = overflow;
    }
    if overflow > *live_frozen {
        insert_scrollback_lines(terminal, &live_lines[*live_frozen..overflow], theme)?;
        *live_frozen = overflow;
    }
    Ok(())
}

/// Pi-style hard reset: clear screen + home + clear scrollback, then reprint
/// the welcome banner and the entire committed transcript with the current
/// expand state. Content taller than the viewport spills into fresh native
/// scrollback, so the user scrolls with their terminal as usual.
#[allow(clippy::too_many_arguments)]
fn reset_and_redraw(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    runtime: &CliRuntime,
    version: &str,
    chat: &[ChatItem],
    flushed: &mut usize,
    live_frozen: &mut usize,
    streaming_assistant: Option<usize>,
    streaming_thinking: Option<usize>,
    expanded: bool,
    hide_thinking: bool,
) -> anyhow::Result<()> {
    use crossterm::cursor::MoveTo;
    use crossterm::terminal::{BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate};

    let mut out = io::stdout();
    // Equivalent of `\x1b[2J\x1b[H\x1b[3J`: without purging scrollback the
    // reprinted transcript would duplicate below the old copy.
    execute!(
        out,
        BeginSynchronizedUpdate,
        Clear(ClearType::All),
        MoveTo(0, 0),
        Clear(ClearType::Purge)
    )?;

    // Re-anchor a fresh inline viewport at the top of the now-empty screen.
    *terminal = Terminal::with_options(
        CrosstermBackend::new(io::stdout()),
        TerminalOptions {
            viewport: Viewport::Inline(FOOTER_HEIGHT),
        },
    )?;
    let _ = terminal.hide_cursor();

    print_welcome(terminal, runtime, version)?;
    *flushed = 0;
    *live_frozen = 0;
    // Fresh scrollback: nothing spilled, so a redraw request cannot occur here.
    let _ = flush_committed(
        terminal,
        chat,
        flushed,
        live_frozen,
        streaming_assistant,
        streaming_thinking,
        &runtime.theme,
        expanded,
        hide_thinking,
    )?;
    let _ = execute!(io::stdout(), EndSynchronizedUpdate);
    Ok(())
}

/// Print the welcome banner + info card into terminal scrollback.
fn print_welcome(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    runtime: &CliRuntime,
    version: &str,
) -> anyhow::Result<()> {
    let endpoint = endpoint_for(runtime);
    let width = terminal.size()?.width;
    let lines = welcome_lines(
        &runtime.theme,
        version,
        runtime.selected_model.as_ref(),
        runtime.model_note.as_deref(),
        &endpoint,
        &runtime.session_id,
        runtime.resources.skills.len(),
        runtime.resources.prompts.len(),
        runtime.needs_provider_setup,
        width,
    );
    terminal.insert_before(lines.len() as u16, |buf| {
        render_lines_to_buffer(&lines, buf, &runtime.theme);
    })?;
    Ok(())
}

/// Dynamic `(name, description)` command entries for skills and prompt templates.
fn dynamic_command_entries(runtime: &CliRuntime) -> Vec<(String, String)> {
    let mut extra: Vec<(String, String)> = runtime
        .resources
        .skills
        .iter()
        .map(|s| {
            let desc = if s.description.is_empty() {
                "skill".to_string()
            } else {
                format!("skill — {}", s.description)
            };
            (format!("skill:{}", s.name), desc)
        })
        .collect();
    extra.extend(runtime.resources.prompts.iter().map(|p| {
        let desc = match &p.argument_hint {
            Some(h) => format!("{h} — prompt template"),
            None => "prompt template".to_string(),
        };
        (p.name.clone(), desc)
    }));
    extra
}

fn path_status_line(cwd: &Path) -> String {
    let home = dirs::home_dir();
    let display = if let Some(home) = home {
        if let Ok(rest) = cwd.strip_prefix(&home) {
            format!("~/{}", rest.display())
        } else {
            cwd.display().to_string()
        }
    } else {
        cwd.display().to_string()
    };
    let branch = git_branch(cwd);
    match branch {
        Some(b) => format!("{display} ({b})"),
        None => display,
    }
}

fn git_branch(cwd: &Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "--abbrev-ref", "HEAD"])
        .current_dir(cwd)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if s.is_empty() || s == "HEAD" {
        None
    } else {
        Some(s)
    }
}

fn sys(text: impl Into<String>) -> ChatItem {
    ChatItem::System { text: text.into() }
}

/// An error line, rendered in the theme's error color.
fn error_item(text: impl Into<String>) -> ChatItem {
    ChatItem::Error { text: text.into() }
}

/// Index of the first queued bubble, or `chat.len()` if none.
/// Agent stream items must be inserted here so they stay above the queue.
fn first_queued_index(chat: &[ChatItem]) -> usize {
    chat.iter()
        .position(|c| matches!(c, ChatItem::Queued { .. }))
        .unwrap_or(chat.len())
}

fn bump_stream_index(slot: &mut Option<usize>, inserted_at: usize) {
    if let Some(i) = slot {
        if *i >= inserted_at {
            *i += 1;
        }
    }
}

/// Push an agent/transcript item above any pending queued user messages.
fn push_before_queued(chat: &mut Vec<ChatItem>, item: ChatItem) -> usize {
    let idx = first_queued_index(chat);
    chat.insert(idx, item);
    idx
}

/// Move any queued bubbles to the end (repairs mid-stream enqueue order) and
/// remap live streaming indices.
fn settle_queue_at_end(
    chat: &mut Vec<ChatItem>,
    streaming_assistant: &mut Option<usize>,
    streaming_thinking: &mut Option<usize>,
) {
    if !chat.iter().any(|c| matches!(c, ChatItem::Queued { .. })) {
        return;
    }
    // Already a clean trailing run of Queued items?
    if let Some(first) = chat
        .iter()
        .position(|c| matches!(c, ChatItem::Queued { .. }))
    {
        if chat[first..]
            .iter()
            .all(|c| matches!(c, ChatItem::Queued { .. }))
        {
            return;
        }
    }

    let mut queued = Vec::new();
    let mut settled = Vec::with_capacity(chat.len());
    let mut map = vec![None; chat.len()];
    for (i, item) in chat.drain(..).enumerate() {
        if matches!(item, ChatItem::Queued { .. }) {
            queued.push(item);
        } else {
            map[i] = Some(settled.len());
            settled.push(item);
        }
    }
    settled.extend(queued);
    *chat = settled;

    if let Some(idx) = *streaming_assistant {
        *streaming_assistant = map.get(idx).copied().flatten();
    }
    if let Some(idx) = *streaming_thinking {
        *streaming_thinking = map.get(idx).copied().flatten();
    }
}

fn truncate_status(s: &str, max: usize) -> String {
    let one_line = s.lines().next().unwrap_or(s).trim();
    let mut out: String = one_line.chars().take(max).collect();
    if one_line.chars().count() > max {
        out.push('…');
    }
    out
}

#[allow(clippy::too_many_arguments)]
async fn drain_ui_events(
    runtime: &mut CliRuntime,
    rx: &mut mpsc::UnboundedReceiver<UiEvent>,
    review_rx: &mut mpsc::UnboundedReceiver<PendingInteraction>,
    active_approval: &mut Option<ActiveApproval>,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
    turn_step: &mut String,
    streaming_assistant: &mut Option<usize>,
    streaming_thinking: &mut Option<usize>,
    working: &mut bool,
    message_queue: &mut VecDeque<QueuedMessage>,
    token_bar: &mut TokenBarState,
    refresh_token_bar: &mut bool,
    tx: &mpsc::UnboundedSender<UiEvent>,
) {
    while let Ok(ev) = rx.try_recv() {
        match ev {
            UiEvent::UpdateAvailable(message) => {
                push_before_queued(chat, sys(message));
            }
            UiEvent::Update(envelope) => {
                // Updates for another session (for example one just left with
                // /new or a fork) must not leak into this transcript.
                if envelope.session.as_str() != runtime.session_id {
                    continue;
                }
                // An approval the backend withdrew (for example on cancel)
                // must not keep capturing keys.
                if let SessionUpdate::InteractionResolved { id, .. } = &envelope.update {
                    if active_approval
                        .as_ref()
                        .is_some_and(|a| &a.interaction == id)
                    {
                        active_approval.take();
                        push_before_queued(chat, sys("approval withdrawn"));
                    }
                }
                let UpdateEnvelope { update, origin, .. } = *envelope;
                handle_update(
                    update,
                    origin,
                    chat,
                    status,
                    turn_step,
                    streaming_assistant,
                    streaming_thinking,
                    working,
                    token_bar,
                    refresh_token_bar,
                );
            }
            UiEvent::TurnFailed(err) => {
                *working = false;
                turn_step.clear();
                chat.push(error_item(format!("error: {err}")));
                *status = "error".into();
                tracing::error!(error = %err, "turn failed");
                if let Some(path) = &runtime.debug_log_path {
                    crate::debug_log::append_note(path, &format!("turn_failed: {err}"));
                }
            }
            UiEvent::CompactDone(result) => {
                *working = false;
                turn_step.clear();
                match result {
                    Ok(msg) => {
                        chat.push(sys(msg));
                        *status = "ready".into();
                        // Compaction invalidates prior context estimates until the next turn.
                        token_bar.context_tokens = None;
                        *refresh_token_bar = true;
                    }
                    Err(e) => {
                        chat.push(sys(format!("compact failed: {e}")));
                        *status = "ready".into();
                    }
                }
            }
            UiEvent::SandboxDone(result) => {
                *working = false;
                turn_step.clear();
                match result {
                    Ok(SandboxDoneOk::Off) => {
                        runtime.settings.sandbox.mode = "off".into();
                        let _ = runtime.save_settings();
                        chat.push(sys("sandbox → off"));
                        *status = "ready".into();
                    }
                    Ok(SandboxDoneOk::Local {
                        isolation,
                        runtime: oci,
                    }) => {
                        runtime.settings.sandbox.mode = "local".into();
                        runtime.settings.sandbox.isolation = isolation.clone();
                        runtime.settings.sandbox.runtime = oci.clone();
                        let _ = runtime.save_settings();
                        chat.push(sys(format!("sandbox → local --{isolation} --{oci}")));
                        *status = "ready".into();
                    }
                    Err(e) => {
                        chat.push(sys(e));
                        *status = "ready".into();
                    }
                }
            }
            #[cfg(feature = "orchestration")]
            UiEvent::WorkflowDone(result) => {
                *working = false;
                turn_step.clear();
                match result {
                    Ok(info) => {
                        let msg = if info.success {
                            format!(
                                "workflow done — {}/{} tasks completed",
                                info.completed_count, info.total_count,
                            )
                        } else {
                            format!(
                                "workflow failed — {} completed, {} failed out of {} tasks",
                                info.completed_count, info.failed_count, info.total_count,
                            )
                        };
                        chat.push(sys(msg));
                        if !info.output.is_empty() {
                            chat.push(ChatItem::Assistant { text: info.output });
                        } else if info.success {
                            chat.push(sys("workflow produced no output"));
                        }
                        if !info.artifacts.is_empty() {
                            chat.push(sys(format!("wrote: {}", info.artifacts.join(", "))));
                        }
                        *status = "ready".into();
                    }
                    Err(e) => {
                        chat.push(sys(format!("workflow error: {e}")));
                        *status = "ready".into();
                    }
                }
            }
            #[cfg(feature = "orchestration")]
            UiEvent::WorkflowGraph { outline, mermaid } => {
                chat.push(sys(format!(
                    "task graph:\n{outline}\n\n```mermaid\n{mermaid}\n```"
                )));
            }
            #[cfg(feature = "orchestration")]
            UiEvent::WorkflowTaskStarted {
                task_id,
                description,
            } => {
                chat.push(ChatItem::WorkflowTask {
                    task_id,
                    description,
                    status: CardStatus::Pending,
                    output: String::new(),
                });
            }
            #[cfg(feature = "orchestration")]
            UiEvent::WorkflowTaskCompleted { task_id, output } => {
                if let Some(idx) = find_workflow_task_index(chat, &task_id) {
                    if let Some(ChatItem::WorkflowTask {
                        status: s,
                        output: o,
                        ..
                    }) = chat.get_mut(idx)
                    {
                        *s = CardStatus::Success;
                        *o = output;
                    }
                }
            }
            #[cfg(feature = "orchestration")]
            UiEvent::WorkflowTaskFailed { task_id, error } => {
                if let Some(idx) = find_workflow_task_index(chat, &task_id) {
                    if let Some(ChatItem::WorkflowTask {
                        status: s,
                        output: o,
                        ..
                    }) = chat.get_mut(idx)
                    {
                        *s = CardStatus::Error;
                        *o = error;
                    }
                }
            }
            UiEvent::McpAuthUrl {
                name,
                url,
                browser_error,
            } => {
                let mut text = format!("Authorize mcp `{name}` in your browser:\n{url}");
                if let Some(err) = browser_error {
                    text.push_str(&format!(
                        "\nCould not open a browser ({err}). Open the link above."
                    ));
                }
                chat.push(sys(text));
                *status = format!("mcp `{name}` · waiting for browser authorization");
            }
            UiEvent::McpLoginDone { name, result } => match result {
                Ok(outcome) => {
                    finish_mcp_login(runtime, chat, status, &name, outcome).await;
                }
                Err(err) => {
                    chat.push(error_item(format!("mcp `{name}` login failed: {err}")));
                    *status = "ready".into();
                }
            },
        }
    }
    while let Ok(prompt) = review_rx.try_recv() {
        if let Some(prev) = active_approval.take() {
            prev.reply.send(ApprovalDecision::Reject {
                reason: Some("superseded by another review".into()),
            });
        }
        if let Some(review) = ActiveApproval::from_interaction(prompt) {
            *status = format!("review · {} · {}", review.kind.label(), review.summary);
            *active_approval = Some(review);
        }
    }
    try_drain_message_queue(
        runtime,
        chat,
        status,
        turn_step,
        streaming_assistant,
        streaming_thinking,
        working,
        message_queue,
        tx,
    );
}

/// Forward the backend's updates into the UI loop, logging each when debug
/// logging is on. Ends when the backend or the UI goes away.
fn forward_updates(
    mut updates: tokio::sync::broadcast::Receiver<UpdateEnvelope>,
    tx: mpsc::UnboundedSender<UiEvent>,
    debug_log: Option<std::path::PathBuf>,
) {
    use tokio::sync::broadcast::error::RecvError;
    tokio::spawn(async move {
        loop {
            match updates.recv().await {
                Ok(envelope) => {
                    if let Some(path) = &debug_log {
                        crate::debug_log::append_note(
                            path,
                            &format!("event {}", update_note(&envelope.update)),
                        );
                    }
                    if tx.send(UiEvent::Update(Box::new(envelope))).is_err() {
                        return;
                    }
                }
                Err(RecvError::Lagged(missed)) => {
                    tracing::warn!(
                        missed,
                        "UI fell behind the agent; some updates were dropped"
                    );
                }
                Err(RecvError::Closed) => return,
            }
        }
    });
}

fn update_note(update: &SessionUpdate) -> String {
    match update {
        SessionUpdate::Notice { text, .. } => format!("notice: {text}"),
        SessionUpdate::ToolCall(call) => format!("tool_call: {}", call.title),
        SessionUpdate::ToolCallUpdate(u) => format!("tool_update: {} {:?}", u.id, u.status),
        SessionUpdate::TurnEnded { stop, .. } => format!("turn_ended: {stop:?}"),
        other => serde_json::to_value(other)
            .ok()
            .and_then(|v| v.get("type").and_then(|t| t.as_str()).map(str::to_owned))
            .unwrap_or_else(|| "update".into()),
    }
}

fn agent_is_busy(runtime: &CliRuntime, working: bool) -> bool {
    working || runtime.backend.is_busy(&runtime.session_ref())
}

fn enqueue_user_message(
    chat: &mut Vec<ChatItem>,
    message_queue: &mut VecDeque<QueuedMessage>,
    display: String,
    prompt: String,
) {
    message_queue.push_back(QueuedMessage {
        display: display.clone(),
        prompt,
    });
    chat.push(ChatItem::Queued { text: display });
}

fn flush_message_queue(chat: &mut Vec<ChatItem>, message_queue: &mut VecDeque<QueuedMessage>) {
    message_queue.clear();
    chat.retain(|item| !matches!(item, ChatItem::Queued { .. }));
}

fn dequeue_last_message(
    chat: &mut Vec<ChatItem>,
    message_queue: &mut VecDeque<QueuedMessage>,
) -> Option<String> {
    let item = message_queue.pop_back()?;
    if let Some(idx) = chat
        .iter()
        .rposition(|c| matches!(c, ChatItem::Queued { text: t } if *t == item.display))
    {
        chat.remove(idx);
    }
    Some(item.display)
}

#[allow(
    clippy::too_many_arguments,
    reason = "pre-existing signature; refactor tracked separately"
)]
fn start_user_turn(
    runtime: &CliRuntime,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
    turn_step: &mut String,
    streaming_assistant: &mut Option<usize>,
    streaming_thinking: &mut Option<usize>,
    working: &mut bool,
    display: String,
    prompt: String,
    tx: &mpsc::UnboundedSender<UiEvent>,
) {
    // Keep prior turn output above this user message.
    settle_queue_at_end(chat, streaming_assistant, streaming_thinking);

    // Promote a matching queued bubble if this came from the outbound queue.
    if let Some(idx) = chat
        .iter()
        .position(|item| matches!(item, ChatItem::Queued { text: t } if *t == display))
    {
        chat[idx] = ChatItem::User { text: display };
    } else {
        chat.push(ChatItem::User { text: display });
    }
    *working = true;
    *turn_step = "starting…".into();
    *streaming_assistant = None;
    if let Some(idx) = streaming_thinking.take() {
        if let Some(ChatItem::Thinking { done, .. }) = chat.get_mut(idx) {
            *done = true;
        }
    }
    *status = "Working…".into();
    let backend = Arc::clone(&runtime.backend);
    let session = runtime.session_ref();
    let tx = tx.clone();
    if let Some(path) = &runtime.debug_log_path {
        crate::debug_log::append_note(path, "turn: prompt submitted");
    }
    tokio::spawn(async move {
        match backend
            .prompt(&session, vec![ContentBlock::text(prompt)])
            .await
        {
            // The update stream reports the turn, including failures.
            Ok(turn) => {
                if turn.outcome().await == TurnOutcome::Uncertain {
                    let _ = tx.send(UiEvent::TurnFailed(
                        "lost contact with the agent; the turn may be incomplete".into(),
                    ));
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "prompt failed");
                let _ = tx.send(UiEvent::TurnFailed(e.to_string()));
            }
        }
    });
}

#[allow(
    clippy::too_many_arguments,
    reason = "pre-existing signature; refactor tracked separately"
)]
fn try_drain_message_queue(
    runtime: &CliRuntime,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
    turn_step: &mut String,
    streaming_assistant: &mut Option<usize>,
    streaming_thinking: &mut Option<usize>,
    working: &mut bool,
    message_queue: &mut VecDeque<QueuedMessage>,
    tx: &mpsc::UnboundedSender<UiEvent>,
) {
    // Wait until the harness is fully idle. AgentEnd clears `working` slightly
    // before phase flips, and Esc must be able to flush the queue in between.
    if *working || runtime.backend.is_busy(&runtime.session_ref()) || message_queue.is_empty() {
        return;
    }
    let Some(item) = message_queue.pop_front() else {
        return;
    };
    start_user_turn(
        runtime,
        chat,
        status,
        turn_step,
        streaming_assistant,
        streaming_thinking,
        working,
        item.display,
        item.prompt,
        tx,
    );
}

#[allow(
    clippy::too_many_arguments,
    reason = "pre-existing signature; refactor tracked separately"
)]
fn submit_user_text(
    runtime: &CliRuntime,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
    turn_step: &mut String,
    streaming_assistant: &mut Option<usize>,
    streaming_thinking: &mut Option<usize>,
    working: &mut bool,
    message_queue: &mut VecDeque<QueuedMessage>,
    text: String,
    tx: &mpsc::UnboundedSender<UiEvent>,
) {
    if runtime.selected_model.is_none() {
        // Nothing can answer; don't queue or record the message.
        chat.push(error_item(
            loop_agent::harness::AgentHarnessError::NoModelSelected.to_string(),
        ));
        *status = NO_MODEL_LABEL.into();
        return;
    }
    if agent_is_busy(runtime, *working) {
        enqueue_user_message(chat, message_queue, text.clone(), text);
        let n = message_queue.len();
        *status = if n == 1 {
            "queued · will send when ready".into()
        } else {
            format!("{n} messages queued")
        };
    } else {
        start_user_turn(
            runtime,
            chat,
            status,
            turn_step,
            streaming_assistant,
            streaming_thinking,
            working,
            text.clone(),
            text,
            tx,
        );
    }
}

/// Run `!command` locally: show output in the transcript, never send to the LLM
/// or append to the session message list.
async fn run_bang_command(
    runtime: &CliRuntime,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
    streaming_assistant: &mut Option<usize>,
    streaming_thinking: &mut Option<usize>,
    command: &str,
) {
    let command = command.trim();
    if command.is_empty() {
        let idx = push_before_queued(chat, sys("usage: !command"));
        bump_stream_index(streaming_assistant, idx);
        bump_stream_index(streaming_thinking, idx);
        *status = "ready".into();
        return;
    }

    *status = format!("running · !{command}");
    let Some(shell) = runtime.backend.local_shell() else {
        let idx = push_before_queued(
            chat,
            sys("shell commands are not available with this agent"),
        );
        bump_stream_index(streaming_assistant, idx);
        bump_stream_index(streaming_thinking, idx);
        *status = "shell unavailable".into();
        return;
    };
    let item = match shell.run(&runtime.session_ref(), command).await {
        Ok(out) => {
            let combined = if out.stderr.is_empty() {
                out.stdout
            } else if out.stdout.is_empty() {
                out.stderr
            } else {
                format!("{}\n{}", out.stdout, out.stderr)
            };
            *status = if out.exit_code == 0 {
                "done".into()
            } else {
                format!("exit {}", out.exit_code)
            };
            ChatItem::Shell {
                command: command.to_string(),
                output: combined,
                exit_code: Some(out.exit_code),
            }
        }
        Err(e) => {
            *status = "shell failed".into();
            ChatItem::Shell {
                command: command.to_string(),
                output: format!("error: {e}"),
                exit_code: None,
            }
        }
    };
    let idx = push_before_queued(chat, item);
    bump_stream_index(streaming_assistant, idx);
    bump_stream_index(streaming_thinking, idx);
}

/// Gap after a burst that ends on Enter, in case the rest of a paste is still arriving.
const PASTE_BURST_GAP: Duration = Duration::from_millis(10);

fn read_input_batch() -> io::Result<Vec<Event>> {
    let mut batch = Vec::new();
    let mut waits = 0u8;
    loop {
        batch.push(event::read()?);
        while event::poll(Duration::from_millis(0))? {
            batch.push(event::read()?);
        }
        let extend = waits < 20
            && unbracketed_paste_keys(&batch).is_some_and(|keys| paste_burst_pending(&keys))
            && event::poll(PASTE_BURST_GAP)?;
        if !extend {
            break;
        }
        waits += 1;
    }
    Ok(batch)
}

/// Keypresses in `events` when every event could belong to an unbracketed paste.
/// `None` when the batch contains a real paste event or anything else we should
/// dispatch normally.
fn unbracketed_paste_keys(events: &[Event]) -> Option<Vec<KeyEvent>> {
    let mut keys = Vec::new();
    for ev in events {
        match ev {
            Event::Key(key) if key.kind == KeyEventKind::Press => keys.push(*key),
            Event::Key(_) | Event::Resize(_, _) | Event::FocusGained | Event::FocusLost => {}
            _ => return None,
        }
    }
    Some(keys)
}

fn note_terminal_resize(
    terminal: &mut Terminal<CrosstermBackend<Stdout>>,
    width: u16,
    last_width: &mut u16,
    redraw_request: &mut bool,
) {
    let _ = terminal.autoresize();
    // Re-wrapping invalidates the whole transcript.
    if width != *last_width {
        *last_width = width;
        *redraw_request = true;
    }
}

/// Insert pasted text without submitting. Newlines stay in the composer so one
/// Enter sends the whole paragraph.
#[allow(
    clippy::too_many_arguments,
    reason = "pre-existing signature; refactor tracked separately"
)]
fn insert_pasted_text(
    text: &str,
    runtime: &CliRuntime,
    input: &mut InputBuffer,
    pending_setup: &Option<SetupPrompt>,
    model_picker: &mut Option<ModelPickerState>,
    fork_picker: &mut Option<ForkPickerState>,
    skill_picker: &mut Option<SkillPickerState>,
    mcp_picker: &mut Option<McpPickerState>,
    active_approval: &mut Option<ActiveApproval>,
) {
    if let Some(review) = active_approval.as_mut() {
        if review.reason_focused {
            let line = pasted_single_line(text);
            if !line.is_empty() {
                review.reason.push_str(&line);
            }
        }
        return;
    }
    if let Some(picker) = model_picker.as_mut() {
        let line = pasted_single_line(text);
        if !line.is_empty() {
            picker.query.push_str(&line);
            picker.refilter(&runtime.models);
        }
        return;
    }
    if let Some(picker) = fork_picker.as_mut() {
        let line = pasted_single_line(text);
        if !line.is_empty() {
            picker.query.push_str(&line);
            picker.refilter();
        }
        return;
    }
    if let Some(picker) = skill_picker.as_mut() {
        let line = pasted_single_line(text);
        if !line.is_empty() {
            picker.query.push_str(&line);
            picker.refilter();
        }
        return;
    }
    if let Some(picker) = mcp_picker.as_mut() {
        let line = pasted_single_line(text);
        if !line.is_empty() {
            picker.query.push_str(&line);
            picker.refilter();
        }
        return;
    }
    if let Some(prompt) = pending_setup.as_ref() {
        if prompt.options().is_some() {
            return;
        }
        let line = pasted_single_line(text);
        if !line.is_empty() {
            input.insert_str(&line);
        }
        return;
    }
    let block = normalize_pasted_text(text);
    if !block.is_empty() {
        input.insert_str(&block);
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "pre-existing signature; refactor tracked separately"
)]
async fn handle_key(
    key: crossterm::event::KeyEvent,
    runtime: &mut CliRuntime,
    input: &mut InputBuffer,
    history: &mut CommandHistory,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
    turn_step: &mut String,
    clear_presses: &mut u8,
    last_clear: &mut Instant,
    last_escape: &mut Instant,
    streaming_assistant: &mut Option<usize>,
    streaming_thinking: &mut Option<usize>,
    expand_details: &mut bool,
    redraw_request: &mut bool,
    purge_ui_events: &mut bool,
    hide_thinking: &mut bool,
    pending_setup: &mut Option<SetupPrompt>,
    model_picker: &mut Option<ModelPickerState>,
    fork_picker: &mut Option<ForkPickerState>,
    skill_picker: &mut Option<SkillPickerState>,
    mcp_picker: &mut Option<McpPickerState>,
    active_approval: &mut Option<ActiveApproval>,
    working: &mut bool,
    message_queue: &mut VecDeque<QueuedMessage>,
    should_quit: &mut bool,
    ac_entries: &[AutocompleteEntry],
    file_ac_entries: &[FileEntry],
    at_mention: Option<&crate::tui::AtMention>,
    ac_selected: &mut usize,
    tx: &mpsc::UnboundedSender<UiEvent>,
    token_bar: &mut TokenBarState,
    refresh_token_bar: &mut bool,
) -> anyhow::Result<()> {
    if let Some(review) = active_approval.as_mut() {
        match key.code {
            KeyCode::Esc => {
                if review.reason_focused {
                    review.reason_focused = false;
                } else if let Some(r) = active_approval.take() {
                    r.reply.send(ApprovalDecision::Reject { reason: None });
                    chat.push(sys(format!("rejected {} · {}", r.kind.label(), r.summary)));
                    *status = "rejected · continuing".into();
                }
            }
            KeyCode::Up | KeyCode::Left => {
                review.selected = review.selected.saturating_sub(1);
                if review.selected != 2 {
                    review.reason_focused = false;
                }
            }
            KeyCode::Down | KeyCode::Right => {
                if review.selected < 2 {
                    review.selected += 1;
                }
            }
            KeyCode::Tab => {
                review.selected = 2;
                review.reason_focused = true;
            }
            KeyCode::Enter => {
                if let Some(r) = active_approval.take() {
                    let decision = if r.reason_focused || r.selected == 2 {
                        let reason = r.reason.trim().to_string();
                        ApprovalDecision::Reject {
                            reason: (!reason.is_empty()).then_some(reason),
                        }
                    } else if r.selected == 1 {
                        ApprovalDecision::AcceptSession
                    } else {
                        ApprovalDecision::Accept
                    };
                    match &decision {
                        ApprovalDecision::Accept => {
                            chat.push(sys(format!("accepted {} · {}", r.kind.label(), r.summary)));
                            *status = "accepted · continuing".into();
                        }
                        ApprovalDecision::AcceptSession => {
                            chat.push(sys(format!(
                                "accepted all {} for this session",
                                r.kind.label()
                            )));
                            *status = "session auto-approve on · continuing".into();
                        }
                        ApprovalDecision::Reject { reason } => {
                            if let Some(why) = reason {
                                chat.push(sys(format!(
                                    "rejected {} · {} · {why}",
                                    r.kind.label(),
                                    r.summary
                                )));
                            } else {
                                chat.push(sys(format!(
                                    "rejected {} · {}",
                                    r.kind.label(),
                                    r.summary
                                )));
                            }
                            *status = "rejected · continuing".into();
                        }
                    }
                    r.reply.send(decision);
                }
            }
            KeyCode::Backspace if review.reason_focused => {
                review.reason.pop();
            }
            KeyCode::Char(c) if review.reason_focused => {
                if !c.is_control() {
                    review.reason.push(c);
                }
            }
            KeyCode::Char('a') | KeyCode::Char('A') if !review.reason_focused => {
                review.selected = 0;
            }
            KeyCode::Char('s') | KeyCode::Char('S') if !review.reason_focused => {
                review.selected = 1;
            }
            KeyCode::Char('r') | KeyCode::Char('R') if !review.reason_focused => {
                review.selected = 2;
            }
            _ => {}
        }
        return Ok(());
    }

    if let Some(picker) = model_picker.as_mut() {
        match key.code {
            KeyCode::Esc => {
                *model_picker = None;
                *status = "cancelled".into();
            }
            KeyCode::Up => {
                picker.selected = picker.selected.saturating_sub(1);
            }
            KeyCode::Down => {
                if picker.selected + 1 < picker.filtered.len() {
                    picker.selected += 1;
                }
            }
            KeyCode::Enter => {
                if let Some(id) = picker.filtered.get(picker.selected).cloned() {
                    if let Some((provider, model)) = id.split_once('/') {
                        choose_model(runtime, token_bar, provider, model, chat).await;
                    }
                }
                *model_picker = None;
            }
            KeyCode::Char(c) => {
                picker.query.push(c);
                picker.refilter(&runtime.models);
            }
            KeyCode::Backspace => {
                picker.query.pop();
                picker.refilter(&runtime.models);
            }
            _ => {}
        }
        return Ok(());
    }

    if let Some(picker) = fork_picker.as_mut() {
        match key.code {
            KeyCode::Esc => {
                *fork_picker = None;
                *status = "cancelled".into();
            }
            KeyCode::Up => {
                picker.selected = picker.selected.saturating_sub(1);
            }
            KeyCode::Down => {
                if picker.selected + 1 < picker.filtered.len() {
                    picker.selected += 1;
                }
            }
            KeyCode::Enter => {
                let selected = picker.selected_point().cloned();
                *fork_picker = None;
                if let Some(point) = selected {
                    adopt_forked_session(
                        runtime,
                        chat,
                        status,
                        working,
                        message_queue,
                        streaming_assistant,
                        streaming_thinking,
                        redraw_request,
                        purge_ui_events,
                        input,
                        SessionForkSelection::BeforeEntry,
                        Some(point.entry_id.as_str()),
                        None,
                        Some(point.text),
                        token_bar,
                        refresh_token_bar,
                    )
                    .await;
                }
            }
            KeyCode::Char(c) => {
                picker.query.push(c);
                picker.refilter();
            }
            KeyCode::Backspace => {
                picker.query.pop();
                picker.refilter();
            }
            _ => {}
        }
        return Ok(());
    }

    if let Some(picker) = skill_picker.as_mut() {
        match key.code {
            KeyCode::Esc => {
                *skill_picker = None;
                *status = "ready".into();
            }
            KeyCode::Up => {
                picker.selected = picker.selected.saturating_sub(1);
            }
            KeyCode::Down => {
                if picker.selected + 1 < picker.filtered.len() {
                    picker.selected += 1;
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(name) = picker.selected_name().map(str::to_string) {
                    let enabled = !runtime.resources.skills.iter().any(|s| s.name == name);
                    set_skill_enabled(runtime, &name, enabled, chat, status).await;
                }
            }
            KeyCode::Char(c) => {
                picker.query.push(c);
                picker.refilter();
            }
            KeyCode::Backspace => {
                picker.query.pop();
                picker.refilter();
            }
            _ => {}
        }
        return Ok(());
    }

    if let Some(picker) = mcp_picker.as_mut() {
        match key.code {
            KeyCode::Esc => {
                *mcp_picker = None;
                *status = "ready".into();
            }
            KeyCode::Up => {
                picker.selected = picker.selected.saturating_sub(1);
            }
            KeyCode::Down => {
                if picker.selected + 1 < picker.filtered.len() {
                    picker.selected += 1;
                }
            }
            KeyCode::Enter | KeyCode::Char(' ') => {
                if let Some(name) = picker.selected_name().map(str::to_string) {
                    let enabled = runtime
                        .settings
                        .disabled_mcp_servers
                        .iter()
                        .any(|n| n == &name);
                    set_mcp_enabled(runtime, &name, enabled, chat, status).await;
                }
            }
            KeyCode::Char(c) => {
                picker.query.push(c);
                picker.refilter();
            }
            KeyCode::Backspace => {
                picker.query.pop();
                picker.refilter();
            }
            _ => {}
        }
        return Ok(());
    }

    // Slash-command picker navigation (before general keybindings).
    // While a recalled entry is on screen, Up/Down keep walking lines and
    // history. Otherwise `/login` and `/mcp` open this menu and swallow Up.
    if !ac_entries.is_empty() && !history.is_browsing() {
        match key.code {
            KeyCode::Up => {
                *ac_selected = ac_selected.saturating_sub(1);
                return Ok(());
            }
            KeyCode::Down => {
                if *ac_selected + 1 < ac_entries.len() {
                    *ac_selected += 1;
                }
                return Ok(());
            }
            KeyCode::Tab => {
                if let Some(sel) = ac_entries.get(*ac_selected) {
                    input.set(format!("{} ", sel.name));
                }
                return Ok(());
            }
            KeyCode::Enter
                if !key.modifiers.contains(KeyModifiers::SHIFT)
                    && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                if let Some(sel) = ac_entries.get(*ac_selected) {
                    // Prefer the highlighted command when input is still a partial `/…`.
                    if input.as_str().starts_with('/') && !input.as_str().contains(' ') {
                        input.set(sel.name.clone());
                    }
                }
                // Fall through to Submit via keybindings below.
            }
            _ => {}
        }
    }

    // `@file` mention picker — Tab/Enter insert absolute path in place of `@…`.
    // Same history exception as the slash menu: recalled lines keep Up/Down.
    if !file_ac_entries.is_empty() && !history.is_browsing() {
        match key.code {
            KeyCode::Up => {
                *ac_selected = ac_selected.saturating_sub(1);
                return Ok(());
            }
            KeyCode::Down => {
                if *ac_selected + 1 < file_ac_entries.len() {
                    *ac_selected += 1;
                }
                return Ok(());
            }
            KeyCode::Tab => {
                if let (Some(mention), Some(sel)) = (at_mention, file_ac_entries.get(*ac_selected))
                {
                    let text = format!("{} ", insert_text(&sel.absolute));
                    input.replace_char_range(mention.start, mention.end, &text);
                }
                return Ok(());
            }
            KeyCode::Enter
                if !key.modifiers.contains(KeyModifiers::SHIFT)
                    && !key.modifiers.contains(KeyModifiers::CONTROL) =>
            {
                if let (Some(mention), Some(sel)) = (at_mention, file_ac_entries.get(*ac_selected))
                {
                    let text = insert_text(&sel.absolute);
                    input.replace_char_range(mention.start, mention.end, &text);
                }
                // Insert only — stay in the editor so the user can keep typing.
                return Ok(());
            }
            _ => {}
        }
    }

    if let Some(prompt) = pending_setup.clone() {
        if key.code == KeyCode::Esc
            || matches!(
                runtime.keybindings.resolve(key),
                Some(Action::Interrupt | Action::Clear)
            )
        {
            if prompt.esc_quits(runtime.needs_provider_setup) {
                *should_quit = true;
            } else {
                *pending_setup = None;
                input.clear();
                *status = prompt.cancelled_message();
            }
            return Ok(());
        }
        if prompt.options().is_some() {
            // Choice step: arrows move, enter confirms, other keys are ignored.
            let delta = match key.code {
                KeyCode::Up => -1,
                KeyCode::Down => 1,
                KeyCode::Enter => 0,
                _ => return Ok(()),
            };
            if delta != 0 {
                let mut moved = prompt;
                moved.move_selection(delta);
                *pending_setup = Some(moved);
                return Ok(());
            }
        }
        let plain_enter = key.code == KeyCode::Enter
            && !key.modifiers.contains(KeyModifiers::SHIFT)
            && !key.modifiers.contains(KeyModifiers::CONTROL);
        if plain_enter {
            let value = input.as_str().trim().to_string();
            input.clear();
            *pending_setup = None;
            match prompt {
                SetupPrompt::Provider(step) => {
                    advance_provider_setup(
                        step,
                        &value,
                        runtime,
                        pending_setup,
                        input,
                        chat,
                        status,
                    )
                    .await;
                }
                #[cfg(feature = "telemetry")]
                SetupPrompt::Tracing(step) => {
                    advance_tracing_setup(
                        step,
                        &value,
                        runtime,
                        pending_setup,
                        input,
                        chat,
                        status,
                    );
                }
            }
            return Ok(());
        }
        match runtime.keybindings.resolve(key) {
            Some(Action::DeleteBackward) => input.backspace(),
            Some(Action::DeleteForward) => input.delete(),
            Some(Action::MoveLeft) => input.move_left(),
            Some(Action::MoveRight) => input.move_right(),
            Some(Action::MoveWordLeft) => input.move_word_left(),
            Some(Action::MoveWordRight) => input.move_word_right(),
            Some(Action::MoveLineStart) => input.move_line_start(),
            Some(Action::MoveLineEnd) => input.move_line_end(),
            Some(Action::DeleteWordBackward) => input.delete_word_backward(),
            Some(Action::DeleteToLineStart | Action::DeleteLine) => input.clear(),
            Some(Action::NewLine) => {} // ignore newlines while pasting a key
            _ => {
                if let KeyCode::Char(c) = key.code {
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT)
                        && !key.modifiers.contains(KeyModifiers::SUPER)
                        && c != '\n'
                    {
                        input.insert_char(c);
                    }
                }
            }
        }
        return Ok(());
    }

    if let Some(action) = runtime.keybindings.resolve(key) {
        match action {
            Action::Exit if input.is_empty() => {
                *should_quit = true;
                return Ok(());
            }
            Action::Clear => {
                if last_clear.elapsed() < Duration::from_secs(1) {
                    *clear_presses += 1;
                } else {
                    *clear_presses = 1;
                }
                *last_clear = Instant::now();
                if *clear_presses >= 2 {
                    *should_quit = true;
                } else if input.is_empty() {
                    *status = "ctrl+c again to quit".into();
                } else {
                    input.clear();
                    history.reset_browse();
                }
                return Ok(());
            }
            Action::Interrupt => {
                let had_work = agent_is_busy(runtime, *working);
                let had_queue = !message_queue.is_empty();
                if had_work {
                    runtime.backend.cancel(&runtime.session_ref());
                }
                if had_queue {
                    flush_message_queue(chat, message_queue);
                }
                if had_work || had_queue {
                    *working = false;
                    turn_step.clear();
                    *streaming_assistant = None;
                    if let Some(idx) = streaming_thinking.take() {
                        if let Some(ChatItem::Thinking { done, .. }) = chat.get_mut(idx) {
                            *done = true;
                        }
                    }
                    *status = if had_queue {
                        "interrupted · queue cleared".into()
                    } else {
                        "interrupted".into()
                    };
                } else if last_escape.elapsed() < Duration::from_millis(500) {
                    match runtime.settings.double_escape_action.as_str() {
                        "tree" => {
                            chat.push(sys("session tree: use /tree (branch nav in session store)"));
                        }
                        "fork" => {
                            open_fork_picker(runtime, fork_picker, model_picker, chat, status)
                                .await;
                        }
                        _ => {}
                    }
                }
                *last_escape = Instant::now();
                return Ok(());
            }
            Action::Submit => {
                let line = input.as_str().trim().to_string();
                history.push(&line);
                input.clear();
                if line.is_empty() {
                    return Ok(());
                }
                if let Some(command) = line.strip_prefix('!') {
                    run_bang_command(
                        runtime,
                        chat,
                        status,
                        streaming_assistant,
                        streaming_thinking,
                        command,
                    )
                    .await;
                } else if line.starts_with('/') {
                    let skill_names: Vec<_> = runtime
                        .resources
                        .skills
                        .iter()
                        .map(|s| s.name.clone())
                        .collect();
                    let template_names: Vec<_> = runtime
                        .resources
                        .prompts
                        .iter()
                        .map(|p| p.name.clone())
                        .collect();
                    if let Some(cmd) = commands::parse_command(&line) {
                        let effect = commands::dispatch(&cmd, &skill_names, &template_names);
                        *should_quit = apply_effect(
                            effect,
                            runtime,
                            chat,
                            status,
                            turn_step,
                            pending_setup,
                            model_picker,
                            fork_picker,
                            skill_picker,
                            mcp_picker,
                            hide_thinking,
                            working,
                            message_queue,
                            streaming_assistant,
                            streaming_thinking,
                            redraw_request,
                            purge_ui_events,
                            input,
                            tx,
                            token_bar,
                            refresh_token_bar,
                        )
                        .await?;
                    }
                } else {
                    submit_user_text(
                        runtime,
                        chat,
                        status,
                        turn_step,
                        streaming_assistant,
                        streaming_thinking,
                        working,
                        message_queue,
                        line,
                        tx,
                    );
                }
                return Ok(());
            }
            Action::NewLine => {
                input.insert_newline();
                return Ok(());
            }
            Action::MoveLeft => {
                input.move_left();
                return Ok(());
            }
            Action::MoveRight => {
                input.move_right();
                return Ok(());
            }
            Action::MoveUp => {
                if !input.move_up() {
                    if let Some(text) = history.previous(input.as_str()) {
                        input.set(text);
                    }
                }
                return Ok(());
            }
            Action::MoveDown => {
                if !input.move_down() {
                    if let Some(text) = history.next_entry() {
                        input.set(text);
                    }
                }
                return Ok(());
            }
            Action::MoveWordLeft => {
                input.move_word_left();
                return Ok(());
            }
            Action::MoveWordRight => {
                input.move_word_right();
                return Ok(());
            }
            Action::MoveLineStart => {
                input.move_line_start();
                return Ok(());
            }
            Action::MoveLineEnd => {
                input.move_line_end();
                return Ok(());
            }
            Action::DeleteBackward => {
                input.backspace();
                return Ok(());
            }
            Action::DeleteForward => {
                input.delete();
                return Ok(());
            }
            Action::DeleteWordBackward => {
                input.delete_word_backward();
                return Ok(());
            }
            Action::DeleteWordForward => {
                input.delete_word_forward();
                return Ok(());
            }
            Action::DeleteToLineStart => {
                input.delete_to_line_start();
                return Ok(());
            }
            Action::DeleteToLineEnd => {
                input.delete_to_line_end();
                return Ok(());
            }
            Action::DeleteLine => {
                input.delete_line();
                return Ok(());
            }
            Action::ModelSelect => {
                *fork_picker = None;
                *model_picker = Some(ModelPickerState::new(runtime.available_models().await));
                return Ok(());
            }
            Action::SessionFork => {
                open_fork_picker(runtime, fork_picker, model_picker, chat, status).await;
                return Ok(());
            }
            Action::ModelCycleForward | Action::ModelCycleBackward => {
                cycle_model(
                    runtime,
                    token_bar,
                    action == Action::ModelCycleForward,
                    chat,
                )
                .await;
                return Ok(());
            }
            Action::ThinkingCycle => {
                cycle_thinking(runtime, chat).await;
                return Ok(());
            }
            Action::ThinkingToggle => {
                *hide_thinking = !*hide_thinking;
                *redraw_request = true;
                *status = if *hide_thinking {
                    "thinking hidden".into()
                } else {
                    "thinking visible".into()
                };
                return Ok(());
            }
            Action::ToolsExpand => {
                *expand_details = !*expand_details;
                *redraw_request = true;
                *status = if *expand_details {
                    "tool output: expanded".into()
                } else {
                    "tool output: collapsed".into()
                };
                return Ok(());
            }
            Action::MessageCopy => {
                copy_last_assistant(chat, status);
                return Ok(());
            }
            Action::ExternalEditor => {
                if let Ok(edited) = external_edit(input.as_str()) {
                    input.set(edited);
                }
                return Ok(());
            }
            Action::FollowUp => {
                let line = input.as_str().trim().to_string();
                if !line.is_empty() {
                    history.push(&line);
                    input.clear();
                    submit_user_text(
                        runtime,
                        chat,
                        status,
                        turn_step,
                        streaming_assistant,
                        streaming_thinking,
                        working,
                        message_queue,
                        line,
                        tx,
                    );
                }
                return Ok(());
            }
            Action::Dequeue => {
                if let Some(text) = dequeue_last_message(chat, message_queue) {
                    let preview = truncate_status(&text, 40);
                    *status = if message_queue.is_empty() {
                        format!("dequeued: {preview}")
                    } else {
                        format!("dequeued: {preview} · {} still queued", message_queue.len())
                    };
                } else {
                    *status = "queue empty".into();
                }
                return Ok(());
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Char(_c) if key.modifiers.contains(KeyModifiers::CONTROL) => {}
        KeyCode::Char(_c) if key.modifiers.contains(KeyModifiers::SUPER) => {}
        KeyCode::Tab => {
            if let Some(sel) = ac_entries.get(*ac_selected).or_else(|| ac_entries.first()) {
                input.set(format!("{} ", sel.name));
            } else if let (Some(mention), Some(sel)) = (
                at_mention,
                file_ac_entries
                    .get(*ac_selected)
                    .or_else(|| file_ac_entries.first()),
            ) {
                let text = format!("{} ", insert_text(&sel.absolute));
                input.replace_char_range(mention.start, mention.end, &text);
            }
        }
        KeyCode::Char(c) => input.insert_char(c),
        _ => {}
    }
    Ok(())
}

struct ModelPickerState {
    all: Vec<String>,
    filtered: Vec<String>,
    selected: usize,
    query: String,
}

impl ModelPickerState {
    /// `models` in picker order (see `Runtime::available_models`).
    fn new(models: Vec<loop_ai::Model>) -> Self {
        let all: Vec<String> = models
            .into_iter()
            .map(|m| format!("{}/{}", m.provider, m.id))
            .collect();
        let filtered = all.clone();
        Self {
            all,
            filtered,
            selected: 0,
            query: String::new(),
        }
    }

    fn refilter(&mut self, _models: &loop_ai::Models) {
        let q = self.query.to_lowercase();
        self.filtered = self
            .all
            .iter()
            .filter(|m| m.to_lowercase().contains(&q))
            .cloned()
            .collect();
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
    }
}

struct ForkPickerState {
    /// Newest-first for display.
    points: Vec<SessionForkPoint>,
    /// Indices into `points` after filter.
    filtered: Vec<usize>,
    selected: usize,
    query: String,
}

impl ForkPickerState {
    fn new(mut points: Vec<SessionForkPoint>) -> Self {
        points.reverse();
        let filtered: Vec<_> = (0..points.len()).collect();
        Self {
            points,
            filtered,
            selected: 0,
            query: String::new(),
        }
    }

    fn refilter(&mut self) {
        let q = self.query.to_lowercase();
        self.filtered = self
            .points
            .iter()
            .enumerate()
            .filter(|(_, p)| {
                p.preview.to_lowercase().contains(&q) || format!("#{}", p.index).contains(&q)
            })
            .map(|(i, _)| i)
            .collect();
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
    }

    fn selected_point(&self) -> Option<&SessionForkPoint> {
        self.filtered
            .get(self.selected)
            .and_then(|&i| self.points.get(i))
    }
}

struct SkillPickerState {
    /// `(name, description)` for every discovered skill, sorted by name.
    skills: Vec<(String, String)>,
    /// Indices into `skills` after filter.
    filtered: Vec<usize>,
    selected: usize,
    query: String,
}

impl SkillPickerState {
    fn new(resources: &crate::resources::LoadedResources) -> Self {
        let skills: Vec<_> = resources
            .all_skills()
            .into_iter()
            .map(|(s, _)| (s.name.clone(), s.description.clone()))
            .collect();
        let filtered = (0..skills.len()).collect();
        Self {
            skills,
            filtered,
            selected: 0,
            query: String::new(),
        }
    }

    fn refilter(&mut self) {
        let q = self.query.to_lowercase();
        self.filtered = self
            .skills
            .iter()
            .enumerate()
            .filter(|(_, (name, desc))| {
                name.to_lowercase().contains(&q) || desc.to_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect();
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
    }

    fn selected_name(&self) -> Option<&str> {
        self.filtered
            .get(self.selected)
            .and_then(|&i| self.skills.get(i))
            .map(|(name, _)| name.as_str())
    }
}

struct McpPickerState {
    /// `(name, how to reach it)` for every configured server, sorted by name.
    servers: Vec<(String, String)>,
    /// Indices into `servers` after filter.
    filtered: Vec<usize>,
    selected: usize,
    query: String,
}

impl McpPickerState {
    fn new(settings: &crate::config::Settings) -> Self {
        let servers = settings
            .mcp_servers
            .iter()
            .map(|(name, cfg)| (name.clone(), mcp_server_hint(cfg)))
            .collect::<Vec<_>>();
        let filtered = (0..servers.len()).collect();
        Self {
            servers,
            filtered,
            selected: 0,
            query: String::new(),
        }
    }

    fn refilter(&mut self) {
        let q = self.query.to_lowercase();
        self.filtered = self
            .servers
            .iter()
            .enumerate()
            .filter(|(_, (name, hint))| {
                name.to_lowercase().contains(&q) || hint.to_lowercase().contains(&q)
            })
            .map(|(i, _)| i)
            .collect();
        if self.selected >= self.filtered.len() {
            self.selected = self.filtered.len().saturating_sub(1);
        }
    }

    fn selected_name(&self) -> Option<&str> {
        self.filtered
            .get(self.selected)
            .and_then(|&i| self.servers.get(i))
            .map(|(name, _)| name.as_str())
    }
}

fn mcp_server_hint(cfg: &loop_app_core::config::settings::McpServerConfig) -> String {
    if let Some(url) = &cfg.url {
        url.clone()
    } else if let Some(command) = &cfg.command {
        if cfg.args.is_empty() {
            command.clone()
        } else {
            format!("{command} {}", cfg.args.join(" "))
        }
    } else {
        "not configured".into()
    }
}

fn start_mcp_login(
    runtime: &CliRuntime,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
    tx: &mpsc::UnboundedSender<UiEvent>,
    name: &str,
) {
    let Some(cfg) = runtime.settings.mcp_servers.get(name) else {
        chat.push(sys(format!("mcp server not found: {name} (see /mcp list)")));
        return;
    };
    let Some(url) = cfg.url.clone() else {
        chat.push(error_item(format!(
            "mcp `{name}` is a local command server and does not use OAuth"
        )));
        return;
    };
    match runtime.mcp_client.oauth_credential_path(name) {
        Ok(Some(_)) => {}
        Ok(None) => {
            chat.push(error_item(
                "OAuth credential storage is not configured for this session".to_string(),
            ));
            return;
        }
        Err(err) => {
            chat.push(error_item(err));
            return;
        }
    }
    chat.push(sys(format!("Starting OAuth login for mcp `{name}`…")));
    *status = format!("mcp `{name}` · waiting for browser authorization");

    let client = Arc::clone(&runtime.mcp_client);
    let tx_url = tx.clone();
    let tx_done = tx.clone();
    let server_name = name.to_string();
    tokio::spawn(async move {
        let name_for_url = server_name.clone();
        let result = client
            .oauth_login(&server_name, &url, move |auth_url| {
                let browser_error = open_browser(auth_url).err().map(|err| err.to_string());
                let _ = tx_url.send(UiEvent::McpAuthUrl {
                    name: name_for_url,
                    url: auth_url.to_string(),
                    browser_error,
                });
            })
            .await;
        let _ = tx_done.send(UiEvent::McpLoginDone {
            name: server_name,
            result,
        });
    });
}

fn open_browser(url: &str) -> std::io::Result<std::process::Child> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("open").arg(url).spawn()
    }
    #[cfg(target_os = "windows")]
    {
        return std::process::Command::new("cmd")
            .args(["/C", "start", "", url])
            .spawn();
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        std::process::Command::new("xdg-open").arg(url).spawn()
    }
}

async fn finish_mcp_login(
    runtime: &mut CliRuntime,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
    name: &str,
    outcome: loop_mcp::OAuthLoginOutcome,
) {
    let disabled = runtime
        .settings
        .disabled_mcp_servers
        .iter()
        .any(|server| server == name);
    let note = match outcome {
        loop_mcp::OAuthLoginOutcome::Authorized => "authorized",
        loop_mcp::OAuthLoginOutcome::NotRequired => "does not require authorization",
    };
    if disabled {
        chat.push(sys(format!(
            "mcp `{name}` {note}. It is disabled; /mcp enable {name} to connect."
        )));
        *status = "ready".into();
        return;
    }
    let Some(entry) = mcp_entry(runtime, name) else {
        chat.push(error_item(format!(
            "mcp `{name}` {note}, but it is no longer configured"
        )));
        *status = "ready".into();
        return;
    };
    runtime.mcp_client.disconnect(name).await;
    match runtime.mcp_client.connect(&entry).await {
        Ok(count) => {
            if let Err(err) = runtime.refresh_mcp_tools().await {
                chat.push(error_item(format!("error setting tools: {err}")));
            }
            let noun = if count == 1 { "tool" } else { "tools" };
            chat.push(sys(format!("mcp `{name}` {note} — {count} {noun}")));
            *status = "ready".into();
        }
        Err(err) => {
            chat.push(error_item(format!(
                "mcp `{name}` {note}, but connecting failed: {err}"
            )));
            *status = "ready".into();
        }
    }
}

fn mcp_entry(runtime: &CliRuntime, name: &str) -> Option<loop_mcp::McpServerEntry> {
    let cfg = runtime.settings.mcp_servers.get(name)?.clone();
    let mut one = std::collections::BTreeMap::new();
    one.insert(name.to_string(), cfg);
    crate::runtime::mcp_server_entries(&one).into_iter().next()
}

async fn logout_mcp(
    runtime: &mut CliRuntime,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
    name: &str,
) {
    if !runtime.settings.mcp_servers.contains_key(name) {
        chat.push(sys(format!("mcp server not found: {name} (see /mcp list)")));
        return;
    }
    match runtime.mcp_client.clear_oauth_credentials(name).await {
        Ok(true) => {
            runtime.mcp_client.disconnect(name).await;
            if let Err(err) = runtime.refresh_mcp_tools().await {
                chat.push(error_item(format!("error setting tools: {err}")));
            }
            chat.push(sys(format!(
                "mcp `{name}` authorization removed. /mcp reload to reconnect."
            )));
            *status = format!("mcp `{name}` logged out");
        }
        Ok(false) => chat.push(sys(format!("mcp `{name}` has no saved authorization"))),
        Err(err) => chat.push(error_item(err)),
    }
}

async fn set_mcp_enabled(
    runtime: &mut CliRuntime,
    name: &str,
    enabled: bool,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
) {
    use crate::runtime::McpToggleOutcome;

    let state = if enabled { "enabled" } else { "disabled" };
    match runtime.set_mcp_server_enabled(name, enabled).await {
        Ok(McpToggleOutcome::Updated) => *status = format!("mcp `{name}` {state}"),
        Ok(McpToggleOutcome::Missing) => {
            chat.push(sys(format!("mcp server not found: {name} (see /mcp list)")));
        }
        Ok(McpToggleOutcome::ConnectFailed(err)) => {
            *status = format!("mcp `{name}` enabled · connect failed");
            chat.push(error_item(format!(
                "mcp `{name}` enabled, but connecting failed: {err}"
            )));
        }
        Err(e) => chat.push(error_item(format!(
            "mcp `{name}` {state} for this session, but saving settings failed: {e:#}"
        ))),
    }
}

async fn mcp_status_item(
    runtime: &CliRuntime,
    kind: &str,
    mark_ok: bool,
    connect_errors: &[(String, String)],
) -> ChatItem {
    let connected = runtime.mcp_client.list_connection_tools().await;
    let mut servers = Vec::new();
    for name in runtime.settings.mcp_servers.keys() {
        let disabled = runtime
            .settings
            .disabled_mcp_servers
            .iter()
            .any(|n| n == name);
        if disabled {
            servers.push(McpServerListing {
                name: name.clone(),
                tools: Vec::new(),
                error: None,
                disabled: true,
            });
            continue;
        }
        if let Some((_, err)) = connect_errors.iter().find(|(n, _)| n == name) {
            servers.push(McpServerListing {
                name: name.clone(),
                tools: Vec::new(),
                error: Some(err.clone()),
                disabled: false,
            });
            continue;
        }
        if let Some((_, tools)) = connected.iter().find(|(n, _)| n == name) {
            servers.push(McpServerListing {
                name: name.clone(),
                tools: tools.clone(),
                error: None,
                disabled: false,
            });
        } else {
            servers.push(McpServerListing {
                name: name.clone(),
                tools: Vec::new(),
                error: Some("not connected".into()),
                disabled: false,
            });
        }
    }
    let disabled_count = servers.iter().filter(|server| server.disabled).count();
    let heading = if kind == "reload" {
        "MCP reload:".into()
    } else if disabled_count == 0 {
        format!("MCP servers ({}):", servers.len())
    } else {
        format!(
            "MCP servers ({} configured, {disabled_count} disabled):",
            servers.len()
        )
    };
    ChatItem::Mcp {
        heading,
        servers,
        mark_ok,
    }
}

async fn set_skill_enabled(
    runtime: &mut CliRuntime,
    name: &str,
    enabled: bool,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
) {
    let state = if enabled { "enabled" } else { "disabled" };
    match runtime.set_skill_enabled(name, enabled).await {
        Ok(true) => *status = format!("skill `{name}` {state}"),
        Ok(false) => chat.push(sys(format!("skill not found: {name} (see /skills list)"))),
        Err(e) => chat.push(error_item(format!(
            "skill `{name}` {state} for this session, but saving settings failed: {e:#}"
        ))),
    }
}

async fn open_fork_picker(
    runtime: &CliRuntime,
    fork_picker: &mut Option<ForkPickerState>,
    model_picker: &mut Option<ModelPickerState>,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
) {
    *model_picker = None;
    match runtime.harness.fork_points().await {
        Ok(points) if points.is_empty() => {
            *fork_picker = None;
            chat.push(sys("no user messages to fork from"));
            *status = "ready".into();
        }
        Ok(points) => {
            *fork_picker = Some(ForkPickerState::new(points));
            *status = "fork · pick a message to edit".into();
        }
        Err(e) => {
            *fork_picker = None;
            chat.push(sys(format!("fork failed: {e}")));
            *status = "ready".into();
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn adopt_forked_session(
    runtime: &mut CliRuntime,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
    working: &mut bool,
    message_queue: &mut VecDeque<QueuedMessage>,
    streaming_assistant: &mut Option<usize>,
    streaming_thinking: &mut Option<usize>,
    redraw_request: &mut bool,
    purge_ui_events: &mut bool,
    input: &mut InputBuffer,
    selection: SessionForkSelection,
    through_entry_id: Option<&str>,
    name: Option<String>,
    draft: Option<String>,
    token_bar: &mut TokenBarState,
    refresh_token_bar: &mut bool,
) {
    message_queue.clear();
    *streaming_assistant = None;
    *streaming_thinking = None;
    *working = false;
    let parent_id = runtime.session_id.clone();
    match runtime
        .harness
        .fork_session(selection, through_entry_id, name)
        .await
    {
        Ok(id) => {
            runtime.native.adopt_session(&id);
            runtime.session_id = id.clone();
            runtime.active_skills.clear();
            *chat = match runtime.backend.transcript(&runtime.session_ref()).await {
                Ok(updates) => chat_from_transcript(updates),
                Err(e) => {
                    tracing::warn!("failed to load forked session transcript: {e}");
                    Vec::new()
                }
            };
            if let Some(text) = draft {
                input.set(text);
                chat.push(sys(format!(
                    "forked from {parent_id} → {id} · edit the message below and submit"
                )));
                *status = "ready · edit forked message".into();
            } else {
                input.clear();
                chat.push(sys(format!("cloned from {parent_id} → {id}")));
                *status = "ready · cloned session".into();
            }
            *redraw_request = true;
            *purge_ui_events = true;
            *refresh_token_bar = true;
            token_bar.sync_window(runtime);
        }
        Err(e) => {
            chat.push(sys(format!("fork failed: {e}")));
            *status = "ready".into();
        }
    }
}

fn endpoint_for(runtime: &CliRuntime) -> String {
    runtime
        .selected_model
        .as_ref()
        .map(|m| m.base_url.clone())
        .filter(|u| !u.is_empty())
        .unwrap_or_else(|| SOKET_BASE_URL.to_string())
}

/// Apply one protocol update to the transcript.
///
/// Live updates also drive the status line and the token bar. Replayed
/// history (`Origin::Replay`) only rebuilds the transcript; it also carries
/// the user's own messages, which live turns show as soon as they are sent.
#[allow(
    clippy::too_many_arguments,
    reason = "pre-existing signature; refactor tracked separately"
)]
fn handle_update(
    update: SessionUpdate,
    origin: Origin,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
    turn_step: &mut String,
    streaming_assistant: &mut Option<usize>,
    streaming_thinking: &mut Option<usize>,
    working: &mut bool,
    token_bar: &mut TokenBarState,
    refresh_token_bar: &mut bool,
) {
    let live = origin == Origin::Live;
    match update {
        SessionUpdate::Notice { level, text } => match level {
            NoticeLevel::Error => {
                push_before_queued(chat, error_item(format!("error: {text}")));
            }
            _ if live => {
                *turn_step = text;
                *working = true;
            }
            _ => {
                push_before_queued(chat, ChatItem::System { text });
            }
        },
        SessionUpdate::TurnStarted { .. } => {
            if live {
                *working = true;
                if turn_step.is_empty() {
                    *turn_step = "thinking…".into();
                }
            }
        }
        SessionUpdate::MessageStarted { role, .. } => {
            // Agent items are created lazily on the first chunk so that
            // thinking and tool items land in true stream order.
            if role == Role::Agent {
                *streaming_assistant = None;
                if live {
                    *turn_step = "waiting for model…".into();
                }
            }
        }
        SessionUpdate::MessageChunk {
            role: Role::User,
            content,
            ..
        } => {
            if let (false, Some(text)) = (live, content.as_text()) {
                push_before_queued(
                    chat,
                    ChatItem::User {
                        text: text.to_owned(),
                    },
                );
            }
        }
        SessionUpdate::MessageChunk {
            role: Role::Agent,
            content,
            ..
        } => {
            let Some(delta) = content.as_text() else {
                return;
            };
            if live {
                *turn_step = "streaming…".into();
            }
            finish_thinking(chat, streaming_thinking);
            let idx = match *streaming_assistant {
                Some(idx) => idx,
                None => {
                    let idx = push_before_queued(
                        chat,
                        ChatItem::Assistant {
                            text: String::new(),
                        },
                    );
                    bump_stream_index(streaming_thinking, idx);
                    *streaming_assistant = Some(idx);
                    idx
                }
            };
            if let Some(ChatItem::Assistant { text }) = chat.get_mut(idx) {
                text.push_str(delta);
            }
        }
        SessionUpdate::ThoughtChunk { content, .. } => {
            let Some(delta) = content.as_text() else {
                return;
            };
            if live {
                *turn_step = "thinking…".into();
            }
            // Later text starts a new item after this thinking block.
            *streaming_assistant = None;
            let idx = match *streaming_thinking {
                Some(idx) => idx,
                None => {
                    let idx = push_before_queued(
                        chat,
                        ChatItem::Thinking {
                            text: String::new(),
                            done: false,
                        },
                    );
                    *streaming_thinking = Some(idx);
                    idx
                }
            };
            if let Some(ChatItem::Thinking { text, .. }) = chat.get_mut(idx) {
                text.push_str(delta);
            }
        }
        SessionUpdate::MessageEnded { .. } => {
            *streaming_assistant = None;
            finish_thinking(chat, streaming_thinking);
            if live {
                settle_queue_at_end(chat, streaming_assistant, streaming_thinking);
            }
        }
        SessionUpdate::ToolCall(call) => {
            let name = call.name.clone().unwrap_or_else(|| call.title.clone());
            if live {
                *turn_step = format!("tool · {name}");
            }
            *streaming_assistant = None;
            finish_thinking(chat, streaming_thinking);
            let summary = match (&call.name, &call.raw_input) {
                (Some(tool), Some(input)) => tool_args_summary(tool, input),
                _ => call.title.clone(),
            };
            let output = tool_text(&call.content);
            let detail = if output.is_empty() {
                call.raw_input
                    .as_ref()
                    .map(|input| serde_json::to_string_pretty(input).unwrap_or_default())
                    .unwrap_or_default()
            } else {
                output
            };
            upsert_tool(
                chat,
                streaming_assistant,
                streaming_thinking,
                call.id.as_str(),
                &name,
                summary,
                detail,
                card_status(call.status),
            );
        }
        SessionUpdate::ToolCallUpdate(update) => {
            apply_tool_update(update, live, chat, turn_step);
        }
        SessionUpdate::Usage(usage) => {
            if live {
                token_bar.apply_protocol_usage(&usage);
            }
        }
        SessionUpdate::TurnEnded { stop, error, .. } => {
            if let Some(error) = error {
                push_before_queued(chat, error_item(format!("error: {}", error.message)));
            } else if live && stop == StopReason::Cancelled {
                push_before_queued(chat, sys("interrupted"));
            } else if live && !answered_since_last_prompt(chat) {
                push_before_queued(chat, sys("(no reply)"));
            }
            *streaming_assistant = None;
            finish_thinking(chat, streaming_thinking);
            if live {
                *working = false;
                turn_step.clear();
                *status = if stop == StopReason::Error {
                    "error · see above".into()
                } else {
                    "ready".into()
                };
                // Ensure any content that landed after queued bubbles can flush.
                settle_queue_at_end(chat, streaming_assistant, streaming_thinking);
                *refresh_token_bar = true;
            }
        }
        // Approvals arrive through HostServices, not the update stream; plans
        // are not shown yet; unknown updates come from newer backends.
        SessionUpdate::InteractionRequested { .. }
        | SessionUpdate::InteractionResolved { .. }
        | SessionUpdate::Plan { .. }
        | SessionUpdate::Unknown(_) => {}
    }
}

/// Whether anything from the agent follows the latest user message.
fn answered_since_last_prompt(chat: &[ChatItem]) -> bool {
    chat.iter()
        .rev()
        .take_while(|item| !matches!(item, ChatItem::User { .. }))
        .any(|item| {
            matches!(
                item,
                ChatItem::Assistant { .. }
                    | ChatItem::Thinking { .. }
                    | ChatItem::Tool { .. }
                    | ChatItem::Error { .. }
            )
        })
}

fn apply_tool_update(
    update: loop_protocol::ToolCallUpdate,
    live: bool,
    chat: &mut Vec<ChatItem>,
    turn_step: &mut String,
) {
    let id = update.id.as_str().to_owned();
    let output = update.content.as_deref().map(tool_text).unwrap_or_default();
    let finished = match update.status {
        Some(ToolCallStatus::Completed) => Some(false),
        Some(ToolCallStatus::Failed) => Some(true),
        _ => None,
    };
    let Some(is_error) = finished else {
        // Still running: show partial output until the result arrives.
        if live {
            if let Some(name) = &update.name {
                *turn_step = format!("tool · {name}");
            }
        }
        if let Some(idx) = find_tool_index(chat, &id) {
            if let Some(ChatItem::Tool { detail, status, .. }) = chat.get_mut(idx) {
                *status = CardStatus::Pending;
                if detail.is_empty() && !output.is_empty() {
                    *detail = format!("(partial) {output}");
                }
            }
        }
        return;
    };
    if live {
        *turn_step = if is_error {
            format!("tool · {} failed", update.name.as_deref().unwrap_or("tool"))
        } else {
            "thinking…".into()
        };
    }
    let card = if is_error {
        CardStatus::Error
    } else {
        CardStatus::Success
    };
    let fallback_summary = if is_error { "error" } else { "done" };
    if let Some(idx) = find_tool_index(chat, &id) {
        if let Some(ChatItem::Tool {
            name,
            detail,
            status,
            summary,
            ..
        }) = chat.get_mut(idx)
        {
            if let Some(new_name) = update.name {
                *name = new_name;
            }
            *status = card;
            if !output.is_empty() {
                if detail.is_empty() {
                    *detail = output;
                } else {
                    detail.push_str("\n---\n");
                    detail.push_str(&output);
                }
            }
            if summary.is_empty() {
                *summary = fallback_summary.into();
            }
        }
    } else {
        push_before_queued(
            chat,
            ChatItem::Tool {
                id,
                name: update.name.unwrap_or_else(|| "tool".into()),
                summary: fallback_summary.into(),
                detail: output,
                status: card,
            },
        );
    }
}

/// Text of tool output, one block per line. Diffs and other non-text content
/// are left to dedicated views.
fn tool_text(content: &[ToolContent]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            ToolContent::Content { content } => content.as_text(),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn card_status(status: ToolCallStatus) -> CardStatus {
    match status {
        ToolCallStatus::Completed => CardStatus::Success,
        ToolCallStatus::Failed => CardStatus::Error,
        ToolCallStatus::Pending | ToolCallStatus::InProgress => CardStatus::Pending,
    }
}

fn finish_thinking(chat: &mut [ChatItem], streaming_thinking: &mut Option<usize>) {
    if let Some(idx) = streaming_thinking.take() {
        if let Some(ChatItem::Thinking { done, .. }) = chat.get_mut(idx) {
            *done = true;
        }
    }
}

/// Rebuild a transcript from a backend's history.
pub(crate) fn chat_from_transcript(updates: Vec<SessionUpdate>) -> Vec<ChatItem> {
    let mut chat = Vec::new();
    let (mut status, mut step) = (String::new(), String::new());
    let (mut assistant, mut thinking, mut working, mut refresh) = (None, None, false, false);
    let mut token_bar = TokenBarState {
        total_tokens: 0,
        context_tokens: None,
        context_window: 0,
    };
    for update in updates {
        handle_update(
            update,
            Origin::Replay,
            &mut chat,
            &mut status,
            &mut step,
            &mut assistant,
            &mut thinking,
            &mut working,
            &mut token_bar,
            &mut refresh,
        );
    }
    chat
}

#[allow(
    clippy::too_many_arguments,
    reason = "pre-existing signature; refactor tracked separately"
)]
fn upsert_tool(
    chat: &mut Vec<ChatItem>,
    streaming_assistant: &mut Option<usize>,
    streaming_thinking: &mut Option<usize>,
    id: &str,
    name: &str,
    summary: String,
    detail: String,
    status: CardStatus,
) {
    if let Some(idx) = find_tool_index(chat, id) {
        if let Some(ChatItem::Tool {
            name: n,
            summary: s,
            detail: d,
            status: st,
            ..
        }) = chat.get_mut(idx)
        {
            *n = name.to_string();
            *s = summary;
            if d.is_empty() || detail.len() >= d.len() {
                *d = detail;
            }
            *st = status;
        }
    } else {
        let idx = push_before_queued(
            chat,
            ChatItem::Tool {
                id: id.to_string(),
                name: name.to_string(),
                summary,
                detail,
                status,
            },
        );
        bump_stream_index(streaming_assistant, idx);
        bump_stream_index(streaming_thinking, idx);
    }
}

/// Apply a wizard `Next`/`Retry`: show the next step (or the same one with its error)
/// in the setup box. Returns the request once the wizard is done.
fn step_wizard<Step, Request>(
    transition: crate::wizard::Transition<Step, Request>,
    into_prompt: fn(Step) -> SetupPrompt,
    value: &str,
    pending_setup: &mut Option<SetupPrompt>,
    input: &mut InputBuffer,
    chat: &mut Vec<ChatItem>,
) -> Option<Request> {
    use crate::wizard::Transition;
    match transition {
        Transition::Next { step, prefill } => {
            input.set(prefill);
            *pending_setup = Some(into_prompt(step));
            None
        }
        Transition::Retry { step, error } => {
            let prompt = into_prompt(step);
            if !prompt.masked() {
                input.set(value);
            }
            chat.push(error_item(format!("{}: {error}", prompt.error_prefix())));
            *pending_setup = Some(prompt);
            None
        }
        Transition::Done(request) => Some(request),
    }
}

/// Feed one answer to the `/tracing setup` wizard and apply the result.
#[cfg(feature = "telemetry")]
fn advance_tracing_setup(
    step: TracingSetup,
    value: &str,
    runtime: &mut CliRuntime,
    pending_setup: &mut Option<SetupPrompt>,
    input: &mut InputBuffer,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
) {
    let transition = step.submit(value, &runtime.settings.tracing);
    let Some(request) = step_wizard(
        transition,
        SetupPrompt::Tracing,
        value,
        pending_setup,
        input,
        chat,
    ) else {
        return;
    };
    chat.push(match runtime.setup_tracing(&request) {
        Ok(state) => sys(format!("✓ {}", describe_tracing_status(&state))),
        Err(err) => error_item(format!("tracing setup failed: {err:#}")),
    });
    *status = "ready".into();
}

/// Feed one answer to the `/login` wizard; on the last step, connect the provider.
async fn advance_provider_setup(
    step: ProviderSetup,
    value: &str,
    runtime: &mut CliRuntime,
    pending_setup: &mut Option<SetupPrompt>,
    input: &mut InputBuffer,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
) {
    let Some(request) = step_wizard(
        step.submit(value),
        SetupPrompt::Provider,
        value,
        pending_setup,
        input,
        chat,
    ) else {
        return;
    };
    *status = format!("login · connecting {}…", request.display_name());
    match runtime.connect_provider(&request).await {
        Ok(connected) => {
            chat.push(sys(format!(
                "✓ Connected {} · {} models — /model to choose one",
                connected.name, connected.model_count
            )));
            *status = "ready · /help for commands".into();
        }
        Err(err) => {
            // Keep the wizard open on the key step so the user can try again.
            chat.push(error_item(format!("login: {err:#}")));
            *pending_setup = Some(SetupPrompt::Provider(ProviderSetup::retry_key(&request)));
            *status = "login · check the key and try again".into();
        }
    }
}

/// `/logout [provider]`: returns the line to show in the transcript.
async fn apply_logout(provider: Option<String>, runtime: &mut CliRuntime) -> String {
    let Some(provider) = provider else {
        let connected = runtime.connected_providers();
        return if connected.is_empty() {
            "Usage: /logout <provider> · no providers are connected".into()
        } else {
            format!(
                "Usage: /logout <provider> · connected: {}",
                connected.join(", ")
            )
        };
    };
    match runtime.disconnect_provider(&provider).await {
        Ok(name) => format!("logged out: {name}"),
        Err(err) => format!("logout: {err:#}"),
    }
}

/// `/tracing …`: returns the line to show in the transcript.
#[cfg(feature = "telemetry")]
fn apply_tracing_command(
    command: TracingCommand,
    runtime: &mut CliRuntime,
    pending_setup: &mut Option<SetupPrompt>,
    input: &mut InputBuffer,
) -> String {
    let result = match command {
        TracingCommand::Status => runtime
            .tracing_status()
            .ok_or_else(|| anyhow::anyhow!("tracing is not available in this mode")),
        TracingCommand::Enable => runtime.set_tracing_enabled(true),
        TracingCommand::Disable => runtime.set_tracing_enabled(false),
        TracingCommand::Setup { backend } => {
            let (step, prefill) = TracingSetup::start(backend, &runtime.settings.tracing);
            input.set(prefill);
            *pending_setup = Some(SetupPrompt::Tracing(step));
            return "tracing setup — follow the prompts below · esc cancels".into();
        }
    };
    match result {
        Ok(state) => describe_tracing_status(&state),
        Err(err) => format!("tracing: {err:#}"),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "pre-existing signature; refactor tracked separately"
)]
async fn apply_effect(
    effect: CommandEffect,
    runtime: &mut CliRuntime,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
    turn_step: &mut String,
    pending_setup: &mut Option<SetupPrompt>,
    model_picker: &mut Option<ModelPickerState>,
    fork_picker: &mut Option<ForkPickerState>,
    skill_picker: &mut Option<SkillPickerState>,
    mcp_picker: &mut Option<McpPickerState>,
    hide_thinking: &mut bool,
    working: &mut bool,
    message_queue: &mut VecDeque<QueuedMessage>,
    streaming_assistant: &mut Option<usize>,
    streaming_thinking: &mut Option<usize>,
    redraw_request: &mut bool,
    purge_ui_events: &mut bool,
    input: &mut InputBuffer,
    tx: &mpsc::UnboundedSender<UiEvent>,
    token_bar: &mut TokenBarState,
    refresh_token_bar: &mut bool,
) -> anyhow::Result<bool> {
    match effect {
        CommandEffect::Quit => return Ok(true),
        CommandEffect::Status(s) => {
            if s.contains("Usage: /theme") {
                let dirs = crate::theme::theme_search_dirs(
                    &runtime.agent_dir,
                    runtime
                        .project_trusted
                        .then_some(crate::config::paths::get_project_dir(&runtime.cwd))
                        .as_deref(),
                );
                let list = Theme::list(&dirs);
                chat.push(sys(format!("themes: {}\n{s}", list.join(", "))));
            } else {
                chat.push(sys(s));
            }
        }
        CommandEffect::SetTheme(name) => {
            let dirs = crate::theme::theme_search_dirs(
                &runtime.agent_dir,
                runtime
                    .project_trusted
                    .then_some(crate::config::paths::get_project_dir(&runtime.cwd))
                    .as_deref(),
            );
            match Theme::load(&name, &dirs) {
                Ok(t) => {
                    runtime.theme = t;
                    runtime.settings.theme = name.clone();
                    let _ = runtime.save_settings();
                    chat.push(sys(format!("theme → {name}")));
                    // Reprint scrollback so already-flushed messages pick up the new colors.
                    *redraw_request = true;
                }
                Err(e) => chat.push(sys(format!("theme error: {e}"))),
            }
        }
        CommandEffect::SelectModel(None) => {
            *fork_picker = None;
            *model_picker = Some(ModelPickerState::new(runtime.available_models().await));
        }
        CommandEffect::SelectModel(Some(spec)) => {
            match loop_app_core::model_selection::resolve_model_spec(&runtime.models, &spec) {
                Ok(m) => {
                    choose_model(runtime, token_bar, &m.provider, &m.id, chat).await;
                }
                Err(err) => chat.push(error_item(format!("{err:#}"))),
            }
        }
        CommandEffect::SetSandbox(mode) => {
            if mode.is_empty() || mode.trim() == "status" {
                let info = runtime.harness.sandbox_info().await;
                chat.push(sys(info.format_box()));
                if mode.is_empty() {
                    chat.push(sys(
                        "use /sandbox status|off|local [--full|--partial] [--crun|--runc|--runsc|--krun]",
                    ));
                }
                return Ok(false);
            }
            let parts: Vec<&str> = mode.split_whitespace().collect();
            match parts.as_slice() {
                ["off"] | ["disabled"] => {
                    let harness = Arc::clone(&runtime.harness);
                    let tx = tx.clone();
                    *working = true;
                    *status = "disabling sandbox…".into();
                    chat.push(sys(" "));
                    chat.push(sys("disabling sandbox…"));
                    tokio::spawn(async move {
                        let result = async {
                            harness.clear_sandbox().await;
                            let env = harness
                                .tool_env()
                                .await
                                .map_err(|e| format!("sandbox: {e}"))?;
                            harness
                                .set_tools(build_tools(env))
                                .await
                                .map_err(|e| format!("sandbox: {e}"))?;
                            Ok(SandboxDoneOk::Off)
                        }
                        .await;
                        let _ = tx.send(UiEvent::SandboxDone(result));
                    });
                    return Ok(false);
                }
                ["local", rest @ ..] => {
                    let (isolation, oci_runtime) = match commands::parse_local_sandbox_flags(rest) {
                        Ok(v) => v,
                        Err(e) => {
                            chat.push(sys(e));
                            return Ok(false);
                        }
                    };
                    let harness = Arc::clone(&runtime.harness);
                    let cwd = runtime.cwd.clone();
                    let tx = tx.clone();
                    let iso_s = isolation.as_str().to_string();
                    let rt_s = oci_runtime.as_str().to_string();
                    *working = true;
                    *status = format!("enabling sandbox (--{iso_s} --{rt_s})…");
                    chat.push(sys(" "));
                    chat.push(sys(format!("enabling sandbox (--{iso_s} --{rt_s})…")));
                    tokio::spawn(async move {
                        let result = async {
                            let sb = KrunSandbox::new(KrunSandbox::config_for(
                                cwd,
                                isolation,
                                oci_runtime,
                            ));
                            sb.start().await.map_err(|e| e.to_string())?;
                            let env = sb.env();
                            if let Err(e) = harness.set_tools(build_tools(Arc::clone(&env))).await {
                                let _ = sb.destroy().await;
                                return Err(format!("sandbox: {e}"));
                            }
                            harness
                                .set_sandbox(SandboxMode::Enabled {
                                    sandbox: Arc::new(sb),
                                })
                                .await;
                            Ok(SandboxDoneOk::Local {
                                isolation: iso_s,
                                runtime: rt_s,
                            })
                        }
                        .await;
                        let _ = tx.send(UiEvent::SandboxDone(result));
                    });
                    return Ok(false);
                }
                ["remote", ..] => {
                    chat.push(sys(
                        "remote sandbox is not implemented yet (use /sandbox local …)",
                    ));
                }
                other => {
                    let joined = other.join(" ");
                    chat.push(sys(format!(
                        "unknown sandbox '{joined}' (status|off|local [--full|--partial] [--crun|--runc|--runsc|--krun])"
                    )));
                }
            }
        }
        #[cfg(feature = "telemetry")]
        CommandEffect::Tracing(command) => {
            chat.push(sys(apply_tracing_command(
                command,
                runtime,
                pending_setup,
                input,
            )));
        }
        CommandEffect::Login(provider) => match ProviderSetup::start(provider.as_deref()) {
            Ok(step) => {
                *pending_setup = Some(SetupPrompt::Provider(step));
                input.clear();
                chat.push(sys("login — follow the prompts below · esc cancels"));
            }
            Err(usage) => chat.push(sys(usage)),
        },
        CommandEffect::Logout(provider) => {
            chat.push(sys(apply_logout(provider, runtime).await));
        }
        CommandEffect::NewSession => {
            // Drop queued prompts and live stream markers before swapping sessions.
            message_queue.clear();
            *streaming_assistant = None;
            *streaming_thinking = None;
            *working = false;
            let spec = SessionSpec {
                cwd: Some(runtime.cwd.clone()),
                name: None,
            };
            match runtime.backend.new_session(spec).await {
                Ok(session) => {
                    runtime.session_id = session.native_id;
                    runtime.resumed = false;
                    runtime.active_skills.clear();
                    chat.clear();
                    let policy = ApprovalPolicy::parse(&runtime.settings.file_edit_review);
                    let enabled = policy.asks_for_session(true);
                    if let Some(bridge) = &runtime.tool_approval {
                        bridge.clear_session_grants();
                        bridge.set_policy_active(enabled);
                    }
                    *status = if enabled {
                        "ready · new session · tool approval on (files + bash)".into()
                    } else {
                        "ready · new session".into()
                    };
                    // Clear screen + scrollback and reprint welcome with the new id.
                    *redraw_request = true;
                    *purge_ui_events = true;
                    token_bar.reset(runtime);
                }
                Err(e) => {
                    chat.push(sys(format!("new session failed: {e}")));
                    *status = "ready".into();
                }
            }
        }
        CommandEffect::SetFileReview(arg) => match arg {
            None => {
                let active = runtime
                    .tool_approval
                    .as_ref()
                    .map(|b| b.policy_active())
                    .unwrap_or(false);
                let grants = runtime
                    .tool_approval
                    .as_ref()
                    .map(|b| {
                        let g = b.auto_approve_groups();
                        let mut parts = Vec::new();
                        if g.contains(GROUP_FILE) {
                            parts.push("file");
                        }
                        if g.contains(GROUP_BASH) {
                            parts.push("bash");
                        }
                        if parts.is_empty() {
                            "(none)".into()
                        } else {
                            parts.join(", ")
                        }
                    })
                    .unwrap_or_else(|| "(none)".into());
                let mut perms = String::new();
                for (k, v) in &runtime.settings.tool_permissions {
                    perms.push_str(&format!("\n    {k}: {v}"));
                }
                chat.push(sys(format!(
                        "tool approval policy: {} (session asking: {active})\n  session auto-approve: {grants}\n  /review newSession|always|never\n  settings.toolPermissions:{perms}\n  settings.diffEditor: {}",
                        runtime.settings.file_edit_review,
                        runtime
                            .settings
                            .diff_editor
                            .as_deref()
                            .unwrap_or("(auto: cursor|code)"),
                    )));
            }
            Some(raw) => {
                let policy = ApprovalPolicy::parse(&raw);
                runtime.settings.file_edit_review = policy.as_str().into();
                let enabled = policy.asks_for_session(!runtime.resumed);
                if let Some(bridge) = &runtime.tool_approval {
                    bridge.set_policy_active(enabled);
                    bridge.set_permissions(permissions_from_settings(
                        &runtime.settings.tool_permissions,
                    ));
                }
                let _ = runtime.save_settings();
                chat.push(sys(format!(
                    "tool approval → {} (this session: {})",
                    policy.as_str(),
                    if enabled { "on" } else { "off" }
                )));
            }
        },
        CommandEffect::SetResponseHeaderTimeout(arg) => match arg {
            None => {
                let ms = runtime.harness.response_header_timeout_ms().await;
                let label = if ms == 0 {
                    "off (unlimited)".into()
                } else if ms % 1000 == 0 {
                    format!("{}s", ms / 1000)
                } else {
                    format!("{ms}ms")
                };
                chat.push(sys(format!(
                        "response header timeout (TTFB): {label}\n  /ttfb off|on|60s|120000\n  settings.responseHeaderTimeoutMs: {} (0 = unlimited)",
                        runtime.settings.response_header_timeout_ms
                    )));
            }
            Some(raw) => match parse_response_header_timeout(&raw) {
                Ok(ms) => {
                    runtime.settings.response_header_timeout_ms = ms;
                    runtime.harness.set_response_header_timeout_ms(ms).await;
                    let _ = runtime.save_settings();
                    let label = if ms == 0 {
                        "off (unlimited — for long-running workflows)".into()
                    } else if ms % 1000 == 0 {
                        format!("{}s", ms / 1000)
                    } else {
                        format!("{ms}ms")
                    };
                    chat.push(sys(format!("response header timeout → {label}")));
                }
                Err(e) => {
                    chat.push(sys(e));
                }
            },
        },
        CommandEffect::Compact(instructions) => {
            if runtime.backend.compaction().is_none() {
                chat.push(sys(
                    "this agent compacts its own context; /compact is not available",
                ));
                return Ok(false);
            }
            let backend = Arc::clone(&runtime.backend);
            let session = runtime.session_ref();
            let tx = tx.clone();
            *working = true;
            chat.push(sys(" "));
            chat.push(sys("compacting…"));
            tokio::spawn(async move {
                let Some(compaction) = backend.compaction() else {
                    return;
                };
                let result = match compaction.compact(&session, instructions.as_deref()).await {
                    Ok(r) => Ok(match r.tokens_before {
                        Some(tokens) => format!(
                            "compacted · ~{tokens} tokens before · {}",
                            truncate_status(&r.summary, 80)
                        ),
                        None => format!("compacted · {}", truncate_status(&r.summary, 80)),
                    }),
                    Err(e) => Err(e.to_string()),
                };
                let _ = tx.send(UiEvent::CompactDone(result));
            });
        }
        CommandEffect::CopyLast => copy_last_assistant(chat, status),
        CommandEffect::Hotkeys => {
            let text = hotkey_help()
                .into_iter()
                .map(|(k, d)| format!("{k:28} {d}"))
                .collect::<Vec<_>>()
                .join("\n");
            chat.push(sys(text));
        }
        CommandEffect::Help => {
            let extra = dynamic_command_entries(runtime);
            chat.push(sys(commands::help_text(&extra)));
        }
        CommandEffect::SessionInfo => {
            let stats = match runtime.backend.session_stats() {
                Some(service) => service
                    .stats(&runtime.session_ref())
                    .await
                    .map_err(|e| e.to_string()),
                None => Err("this agent does not report session statistics".into()),
            };
            match stats {
                Ok(stats) => {
                    let mut report = stats.report;
                    report.push_str(&format!(
                        "\nEnvironment\n  Sessions DB: {}\n  Theme: {}\n  Trusted: {}\n  Model: {}\n",
                        runtime.sessions_db.display(),
                        runtime.theme.name,
                        runtime.project_trusted,
                        runtime.selected_model_spec().unwrap_or_else(|| "none".into()),
                    ));
                    chat.push(sys(report));
                }
                Err(e) => {
                    chat.push(sys(format!(
                        "model: {}\nsessions db: {}\ntheme: {}\ntrusted: {}\n(stats error: {e})",
                        runtime
                            .selected_model_spec()
                            .unwrap_or_else(|| "none".into()),
                        runtime.sessions_db.display(),
                        runtime.theme.name,
                        runtime.project_trusted
                    )));
                }
            }
        }
        CommandEffect::Settings => {
            let mut perms = String::new();
            for (k, v) in &runtime.settings.tool_permissions {
                perms.push_str(&format!("\n    {k}: {v}"));
            }
            chat.push(sys(format!(
                "settings ({})\n  model: {}\n  theme: {}\n  thinking: {}\n  sandbox: {}\n  toolApproval: {}\n  toolPermissions:{perms}\n  diffEditor: {}\n  responseHeaderTimeoutMs: {}{}\n  ui: {}",
                crate::config::paths::settings_path(&runtime.agent_dir).display(),
                runtime.selected_model_spec().unwrap_or_else(|| "none".into()),
                runtime.settings.theme,
                runtime.settings.default_thinking_level,
                runtime.settings.sandbox.display(),
                runtime.settings.file_edit_review,
                runtime
                    .settings
                    .diff_editor
                    .as_deref()
                    .unwrap_or("(auto)"),
                runtime.settings.response_header_timeout_ms,
                if runtime.settings.response_header_timeout_ms == 0 {
                    " (off)"
                } else {
                    ""
                },
                runtime.settings.ui_mode,
            )));
        }
        CommandEffect::Trust(arg) => {
            let decision = match arg.as_deref() {
                Some("yes") | Some("y") => true,
                Some("no") | Some("n") => false,
                None => !runtime.project_trusted,
                Some(other) => {
                    chat.push(sys(format!("usage: /trust [yes|no] (got {other})")));
                    return Ok(false);
                }
            };
            let cwd = runtime.cwd.clone();
            runtime.trust.set(&cwd, decision)?;
            runtime.project_trusted = decision;
            chat.push(sys(format!("project trust → {decision}")));
        }
        CommandEffect::Reload => {
            let _ = runtime
                .models
                .refresh(ModelsRefreshOptions {
                    allow_network: Some(true),
                    force: true,
                    provider_id: None,
                })
                .await;
            runtime.resources = crate::resources::load_resources(
                &runtime.agent_dir,
                &runtime.cwd,
                runtime.project_trusted,
                &runtime.settings,
            );
            runtime.sync_harness_resources().await;
            chat.push(sys("reloaded models, skills, prompts, themes"));
        }
        CommandEffect::Skills(SkillsCommand::Enable(name)) => {
            set_skill_enabled(runtime, &name, true, chat, status).await;
            if runtime.resources.skills.iter().any(|s| s.name == name) {
                chat.push(sys(format!("skill `{name}` enabled")));
            }
        }
        CommandEffect::Skills(SkillsCommand::Disable(name)) => {
            set_skill_enabled(runtime, &name, false, chat, status).await;
            if runtime
                .resources
                .disabled_skills
                .iter()
                .any(|s| s.name == name)
            {
                chat.push(sys(format!("skill `{name}` disabled")));
            }
        }
        CommandEffect::Skills(SkillsCommand::Picker)
            if runtime.resources.skills.len() + runtime.resources.disabled_skills.len() > 0 =>
        {
            *model_picker = None;
            *fork_picker = None;
            *mcp_picker = None;
            *skill_picker = Some(SkillPickerState::new(&runtime.resources));
            *status = "skills · toggle on or off".into();
        }
        CommandEffect::Skills(SkillsCommand::Picker | SkillsCommand::List) => {
            let all = runtime.resources.all_skills();
            if all.is_empty() {
                chat.push(sys(format!(
                    "No skills loaded.\n\nAdd folders containing a SKILL.md (with YAML frontmatter: name, description) under:\n  {}/skills\n  ~/.agents/skills or ~/.claude/skills\n  .agents/skills, .claude/skills, or .loop/skills in trusted projects\nor list extra paths under \"skills\" in settings.json,\nthen run /reload.",
                    runtime.agent_dir.display()
                )));
            } else {
                let mut text = format!(
                    "Skills ({} enabled, {} disabled)\n",
                    runtime.resources.skills.len(),
                    runtime.resources.disabled_skills.len()
                );
                for (s, enabled) in all {
                    let desc = if s.description.is_empty() {
                        "(no description)".to_string()
                    } else {
                        s.description.clone()
                    };
                    let state = if enabled { "" } else { " [disabled]" };
                    text.push_str(&format!(
                        "\n  /skill:{}{state} — {}\n      {}",
                        s.name,
                        desc,
                        s.path.display()
                    ));
                }
                text.push_str("\n\nActivate with /skill:<name> [optional args for the input]. Skills stay active until /new; the model sees them under <available_skills> and can read SKILL.md when relevant.\nToggle with /skills (picker) or /skills enable|disable <name>. Disabled skills are hidden from the model.");
                chat.push(sys(text));
            }
        }
        CommandEffect::Resume => {
            chat.push(sys(
                "resume: restart with `loop --resume <session-id>` (picker UI forthcoming)",
            ));
            if !runtime.session_id.is_empty() {
                chat.push(sys(format!("current session id: {}", runtime.session_id)));
            }
        }
        CommandEffect::Tree => {
            chat.push(sys(
                "session tree stored in SQLite — use /fork /clone; full tree UI forthcoming",
            ));
        }
        CommandEffect::SetName(name) => {
            chat.push(sys(format!("session name → {name}")));
        }
        CommandEffect::Export(path) => {
            chat.push(sys(format!("export: path {:?}", path)));
        }
        CommandEffect::Import(path) => {
            chat.push(sys(format!("import not yet wired: {path}")));
        }
        CommandEffect::Changelog => {
            chat.push(sys(
                "Loop 0.1.0 — interactive coding agent by Soket AI\n- Soket dynamic models\n- ratatui CLI\n- skills, themes, sandbox",
            ));
        }
        CommandEffect::ScopedModels => {
            chat.push(sys(format!(
                "enabledModels: {:?}\nEdit settings.json to change Ctrl+P cycle set.",
                runtime.settings.enabled_models
            )));
        }
        CommandEffect::Fork => {
            open_fork_picker(runtime, fork_picker, model_picker, chat, status).await;
        }
        CommandEffect::CloneSession => {
            *fork_picker = None;
            *model_picker = None;
            adopt_forked_session(
                runtime,
                chat,
                status,
                working,
                message_queue,
                streaming_assistant,
                streaming_thinking,
                redraw_request,
                purge_ui_events,
                input,
                SessionForkSelection::All,
                None,
                None,
                None,
                token_bar,
                refresh_token_bar,
            )
            .await;
        }
        CommandEffect::Mcp(McpCommand::Enable(name)) => {
            set_mcp_enabled(runtime, &name, true, chat, status).await;
            if runtime.settings.mcp_servers.contains_key(&name)
                && !runtime
                    .settings
                    .disabled_mcp_servers
                    .iter()
                    .any(|n| n == &name)
            {
                chat.push(sys(format!("mcp `{name}` enabled")));
            }
        }
        CommandEffect::Mcp(McpCommand::Disable(name)) => {
            set_mcp_enabled(runtime, &name, false, chat, status).await;
            if runtime
                .settings
                .disabled_mcp_servers
                .iter()
                .any(|n| n == &name)
            {
                chat.push(sys(format!("mcp `{name}` disabled")));
            }
        }
        CommandEffect::Mcp(McpCommand::Picker) if !runtime.settings.mcp_servers.is_empty() => {
            *model_picker = None;
            *fork_picker = None;
            *skill_picker = None;
            *mcp_picker = Some(McpPickerState::new(&runtime.settings));
            *status = "mcp · toggle on or off".into();
        }
        CommandEffect::Mcp(McpCommand::Picker | McpCommand::List) => {
            if runtime.settings.mcp_servers.is_empty() {
                chat.push(sys(
                    "No MCP servers configured.\n\nAdd them under \"mcpServers\" in settings.json, then /mcp reload.\nToggle with /mcp (picker) or /mcp enable|disable <name>. Disabled servers are not connected.",
                ));
            } else {
                chat.push(mcp_status_item(runtime, "list", false, &[]).await);
                chat.push(sys(
                    "Toggle with /mcp (picker) or /mcp enable|disable <name>. Disabled servers are not connected. ctrl+o lists tools. Authorize a remote server with /mcp login <name>.",
                ));
            }
        }
        CommandEffect::Mcp(McpCommand::Login(name)) => {
            start_mcp_login(runtime, chat, status, tx, &name);
        }
        CommandEffect::Mcp(McpCommand::Logout(name)) => {
            logout_mcp(runtime, chat, status, &name).await;
        }
        CommandEffect::Mcp(McpCommand::Reload) => {
            runtime.mcp_client.disconnect_all().await;
            if runtime.settings.mcp_servers.is_empty() {
                chat.push(sys("No MCP servers configured in settings.json"));
            } else {
                let entries = crate::runtime::enabled_mcp_server_entries(
                    &runtime.settings.mcp_servers,
                    &runtime.settings.disabled_mcp_servers,
                );
                let results = runtime.mcp_client.connect_all(&entries).await;
                let errors: Vec<(String, String)> = results
                    .iter()
                    .filter_map(|(name, result)| {
                        result.as_ref().err().map(|err| (name.clone(), err.clone()))
                    })
                    .collect();
                if let Err(e) = runtime.refresh_mcp_tools().await {
                    chat.push(sys(format!("error setting tools: {e}")));
                }
                chat.push(mcp_status_item(runtime, "reload", true, &errors).await);
            }
        }
        CommandEffect::Skill { name, args } => {
            if runtime.harness.activate_skill(&name).await {
                if !runtime.active_skills.iter().any(|n| n == &name) {
                    runtime.active_skills.push(name.clone());
                }
                let list = runtime.active_skills.join(", ");
                chat.push(sys(format!(
                    "skill `{name}` is active ({list}). Type your message when ready."
                )));
                *status = format!("skill(s) active: {list} · type your message");
                let args = args.trim();
                if !args.is_empty() {
                    let current = input.as_str();
                    if !current.is_empty() && !current.ends_with(|c: char| c.is_whitespace()) {
                        input.insert_str(" ");
                    }
                    input.insert_str(args);
                }
            } else if runtime
                .resources
                .disabled_skills
                .iter()
                .any(|s| s.name == name)
            {
                chat.push(sys(format!(
                    "skill `{name}` is disabled. Enable it with /skills enable {name}"
                )));
            } else {
                chat.push(sys(format!("skill not found: {name}")));
            }
        }
        CommandEffect::Template { name, args } => {
            if let Some(tmpl) = runtime.resources.prompts.iter().find(|p| p.name == name) {
                let prompt = format_prompt_template_invocation(tmpl, &args);
                let display = format!("/{name} {args}");
                if agent_is_busy(runtime, *working) {
                    enqueue_user_message(chat, message_queue, display, prompt);
                    *status = format!("{} messages queued", message_queue.len());
                } else {
                    start_user_turn(
                        runtime,
                        chat,
                        status,
                        turn_step,
                        streaming_assistant,
                        streaming_thinking,
                        working,
                        display,
                        prompt,
                        tx,
                    );
                }
            } else {
                chat.push(sys(format!("template not found: {name}")));
            }
        }
        #[cfg(feature = "orchestration")]
        CommandEffect::Workflow { goal, concurrency } => {
            if agent_is_busy(runtime, *working) {
                chat.push(sys("cannot start workflow while agent is busy"));
            } else {
                start_workflow(runtime, chat, status, working, tx, &goal, concurrency).await;
            }
        }
    }
    let _ = hide_thinking;
    Ok(false)
}

#[cfg(feature = "orchestration")]
async fn start_workflow(
    runtime: &CliRuntime,
    chat: &mut Vec<ChatItem>,
    status: &mut String,
    working: &mut bool,
    tx: &tokio::sync::mpsc::UnboundedSender<UiEvent>,
    goal: &str,
    concurrency: Option<usize>,
) {
    use loop_agent::harness::orchestration::WorkflowProgressEvent;
    use loop_orchestration::scheduler::SchedulerConfig;

    chat.push(sys(format!("starting workflow: {goal}")));
    *working = true;
    *status = "workflow · planning…".into();

    let harness = Arc::clone(&runtime.harness);
    let goal = goal.to_string();

    let config = concurrency.map(|n| SchedulerConfig {
        max_concurrency: n,
        fail_fast: false,
    });

    let (progress_tx, mut progress_rx) =
        tokio::sync::mpsc::unbounded_channel::<WorkflowProgressEvent>();

    let tx_for_progress = tx.clone();
    tokio::spawn(async move {
        while let Some(event) = progress_rx.recv().await {
            let ui_event = match event {
                WorkflowProgressEvent::GraphPlanned { outline, mermaid } => {
                    UiEvent::WorkflowGraph { outline, mermaid }
                }
                WorkflowProgressEvent::TaskStarted {
                    task_id,
                    description,
                } => UiEvent::WorkflowTaskStarted {
                    task_id,
                    description,
                },
                WorkflowProgressEvent::TaskCompleted { task_id, output } => {
                    UiEvent::WorkflowTaskCompleted { task_id, output }
                }
                WorkflowProgressEvent::TaskFailed { task_id, error } => {
                    UiEvent::WorkflowTaskFailed { task_id, error }
                }
            };
            if tx_for_progress.send(ui_event).is_err() {
                break;
            }
        }
    });

    let tx_for_done = tx.clone();
    tokio::spawn(async move {
        let result = harness
            .start_workflow_from_goal(&goal, None, config, Some(progress_tx))
            .await;

        let event = match result {
            Ok(wf_result) => UiEvent::WorkflowDone(Ok(WorkflowDoneOk {
                success: wf_result.success,
                completed_count: wf_result.task_results.len(),
                failed_count: wf_result.failed_tasks.len(),
                total_count: wf_result.total_task_count,
                output: wf_result.output_text(),
                artifacts: wf_result.artifact_paths(),
            })),
            Err(e) => UiEvent::WorkflowDone(Err(e.to_string())),
        };

        let _ = tx_for_done.send(event);
    });
}

async fn cycle_model(
    runtime: &mut CliRuntime,
    token_bar: &mut TokenBarState,
    forward: bool,
    chat: &mut Vec<ChatItem>,
) {
    let models = runtime.available_models().await;
    if models.is_empty() {
        return;
    }
    let current = runtime.selected_model_spec().unwrap_or_default();
    let next = match models
        .iter()
        .position(|m| format!("{}/{}", m.provider, m.id) == current)
    {
        Some(idx) if forward => (idx + 1) % models.len(),
        Some(idx) => (idx + models.len() - 1) % models.len(),
        None => 0,
    };
    let m = models[next].clone();
    choose_model(runtime, token_bar, &m.provider, &m.id, chat).await;
}

/// Footer text when no model is selected.
const NO_MODEL_LABEL: &str = "no model · /model to choose";

/// Select a model (harness, saved settings, token bar) and report it in the transcript.
async fn choose_model(
    runtime: &mut CliRuntime,
    token_bar: &mut TokenBarState,
    provider: &str,
    id: &str,
    chat: &mut Vec<ChatItem>,
) {
    match runtime.select_model(provider, id).await {
        Ok(model) => {
            token_bar.sync_window(runtime);
            chat.push(sys(format!("model → {}/{}", model.provider, model.id)));
        }
        Err(err) => chat.push(error_item(format!("{err:#}"))),
    }
}

async fn cycle_thinking(runtime: &mut CliRuntime, chat: &mut Vec<ChatItem>) {
    let order = [
        AgentThinkingLevel::Off,
        AgentThinkingLevel::Low,
        AgentThinkingLevel::Medium,
        AgentThinkingLevel::High,
    ];
    let cur = runtime.settings.default_thinking_level.to_lowercase();
    let idx = match cur.as_str() {
        "low" => 1,
        "medium" => 2,
        "high" => 3,
        _ => 0,
    };
    let next = order[(idx + 1) % order.len()];
    let label = match next {
        AgentThinkingLevel::Off => "off",
        AgentThinkingLevel::Low => "low",
        AgentThinkingLevel::Medium => "medium",
        AgentThinkingLevel::High => "high",
        _ => "off",
    };
    runtime.harness.set_thinking_level(next).await;
    runtime.settings.default_thinking_level = label.into();
    chat.push(sys(format!("thinking → {label}")));
}

fn copy_last_assistant(chat: &[ChatItem], status: &mut String) {
    let text = chat.iter().rev().find_map(|l| match l {
        ChatItem::Assistant { text: t } if !t.is_empty() => Some(t.clone()),
        _ => None,
    });
    match text {
        Some(t) => match crate::clipboard::copy_text(&t) {
            Ok(()) => *status = "copied".into(),
            Err(e) => *status = format!("clipboard: {e}"),
        },
        None => *status = "nothing to copy".into(),
    }
}

fn external_edit(current: &str) -> anyhow::Result<String> {
    let editor = std::env::var("EDITOR").unwrap_or_else(|_| "vi".into());
    let mut tmp = tempfile::NamedTempFile::new()?;
    use std::io::Write;
    tmp.write_all(current.as_bytes())?;
    let path = tmp.path().to_path_buf();
    let _ = disable_raw_mode();
    let status = std::process::Command::new(&editor).arg(&path).status()?;
    let _ = enable_raw_mode();
    if !status.success() {
        anyhow::bail!("editor exited with error");
    }
    Ok(std::fs::read_to_string(path)?)
}

/// Parse `/ttfb` args: `off`/`0` → unlimited; `on` → 60s; `60s` / `120000` → ms.
fn parse_response_header_timeout(raw: &str) -> Result<u64, String> {
    let s = raw.trim().to_ascii_lowercase();
    if s.is_empty() {
        return Err("usage: /ttfb off|on|60s|120000".into());
    }
    match s.as_str() {
        "off" | "disable" | "disabled" | "unlimited" | "none" => Ok(0),
        "on" | "default" => Ok(60_000),
        _ => {
            if let Some(secs) = s.strip_suffix('s') {
                let secs: u64 = secs
                    .trim()
                    .parse()
                    .map_err(|_| format!("invalid duration: {raw}"))?;
                return Ok(secs.saturating_mul(1000));
            }
            if let Some(ms) = s.strip_suffix("ms") {
                return ms
                    .trim()
                    .parse()
                    .map_err(|_| format!("invalid duration: {raw}"));
            }
            s.parse::<u64>()
                .map_err(|_| format!("usage: /ttfb off|on|60s|120000 (got {raw})"))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use loop_agent::protocol_events::NativeEventMapper;
    use loop_agent::types::{AgentEvent, AgentMessage};
    use loop_agent::{run_agent_loop, stream_fn_from_models, AgentContext, AgentLoopConfig};
    use loop_ai::providers::{faux_provider, FauxResponse, FauxScript};

    use super::*;

    /// Run one prompt through the real agent loop with a scripted provider,
    /// translate its events as NativeBackend does, and feed the updates to
    /// the TUI's handler as the app loop does.
    async fn run_turn(response: FauxResponse) -> (Vec<ChatItem>, String) {
        let script = FauxScript::new();
        script.push(response);
        let models = loop_ai::Models::new();
        models.set_provider(faux_provider(script));
        let model = models.get_model("faux", "faux-model").unwrap();
        let events = Arc::new(parking_lot::Mutex::new(Vec::new()));
        let sink = Arc::clone(&events);
        run_agent_loop(
            vec![AgentMessage::user_text("hi")],
            AgentContext::default(),
            AgentLoopConfig::new(model),
            Arc::new(move |ev: AgentEvent| {
                sink.lock().push(ev);
                Box::pin(async {})
            }),
            None,
            Some(stream_fn_from_models(Arc::new(models))),
        )
        .await
        .unwrap();

        let (mut chat, mut status, mut step) = (Vec::new(), String::new(), String::new());
        let (mut assistant, mut thinking, mut working) = (None, None, false);
        let mut token_bar = TokenBarState {
            total_tokens: 0,
            context_tokens: None,
            context_window: 0,
        };
        let mut refresh = false;
        let mut mapper = NativeEventMapper::new();
        let updates: Vec<_> = events.lock().iter().flat_map(|e| mapper.map(e)).collect();
        for update in updates {
            handle_update(
                update,
                Origin::Live,
                &mut chat,
                &mut status,
                &mut step,
                &mut assistant,
                &mut thinking,
                &mut working,
                &mut token_bar,
                &mut refresh,
            );
        }
        (chat, status)
    }

    fn errors(chat: &[ChatItem]) -> Vec<String> {
        chat.iter()
            .filter_map(|item| match item {
                ChatItem::Error { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[tokio::test]
    async fn failed_model_call_shows_an_error_and_keeps_error_status() {
        let (chat, status) = run_turn(FauxResponse::Error("HTTP 502 Bad Gateway".into())).await;
        assert_eq!(errors(&chat), ["error: HTTP 502 Bad Gateway"]);
        assert_eq!(status, "error · see above");
    }

    fn live(updates: Vec<SessionUpdate>) -> Vec<ChatItem> {
        let mut chat = vec![ChatItem::User { text: "hi".into() }];
        let (mut status, mut step) = (String::new(), String::new());
        let (mut assistant, mut thinking, mut working, mut refresh) = (None, None, false, false);
        let mut token_bar = TokenBarState {
            total_tokens: 0,
            context_tokens: None,
            context_window: 0,
        };
        for update in updates {
            handle_update(
                update,
                Origin::Live,
                &mut chat,
                &mut status,
                &mut step,
                &mut assistant,
                &mut thinking,
                &mut working,
                &mut token_bar,
                &mut refresh,
            );
        }
        chat
    }

    fn system_lines(chat: &[ChatItem]) -> Vec<String> {
        chat.iter()
            .filter_map(|item| match item {
                ChatItem::System { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn cancelled_and_silent_turns_are_marked() {
        let end = |stop| SessionUpdate::TurnEnded {
            turn_id: "t".into(),
            stop,
            error: None,
        };
        let started = || SessionUpdate::TurnStarted {
            turn_id: "t".into(),
        };
        assert_eq!(
            system_lines(&live(vec![started(), end(StopReason::Cancelled)])),
            ["interrupted"]
        );
        assert_eq!(
            system_lines(&live(vec![started(), end(StopReason::EndTurn)])),
            ["(no reply)"]
        );
        let answered = live(vec![
            started(),
            SessionUpdate::MessageChunk {
                message_id: "m".into(),
                role: Role::Agent,
                content: ContentBlock::text("ok"),
            },
            end(StopReason::EndTurn),
        ]);
        assert!(system_lines(&answered).is_empty());
    }

    #[tokio::test]
    async fn successful_turn_shows_no_error() {
        let (chat, status) = run_turn(FauxResponse::Text("hello".into())).await;
        assert!(errors(&chat).is_empty());
        assert_eq!(status, "ready");
        assert!(chat
            .iter()
            .any(|item| matches!(item, ChatItem::Assistant { text } if text == "hello")));
    }
}
