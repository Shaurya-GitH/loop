# External agents & subscriptions: one uniform architecture (Codex first)

## Context
You want Loop to run on your ChatGPT subscription through Codex today, and on other subscription agents tomorrow (Grok, Nous Research, Claude, Gemini, …), **with no bespoke harness or event code per vendor**. CI work is paused; the parked CI backlog is at the end.

This plan replaces the earlier Codex-specific design (a `CodexBackend` that mapped app-server items straight onto `AgentEvent`). That design made Codex a special case, and every new vendor would have needed its own adapter.

### Research summary
- **How other tools get subscription access.** They use one of five approaches ([yepanywhere](https://yepanywhere.com/subscription-access-approaches)):
  - Raw PTY passthrough. No structured data.
  - Official SDK/protocol per vendor (Yep Anywhere, T3 Code). Works, but each vendor needs an adapter of about 500+ lines.
  - Public OAuth as a model provider (OpenCode, for ChatGPT and Copilot only).
  - Reverse-engineered OAuth (pi-mono). High terms-of-service risk; bans reported.
  - API keys only.
- **T3 Code** writes one adapter per vendor (Codex app-server, Claude Agent SDK, OpenCode SDK, **ACP for Cursor and Grok**) behind a common event model ([deep dive](https://flaviocopes.com/t3-code/), [providers.md](https://github.com/pingdotgg/t3code/blob/main/docs/internals/providers.md)). Lessons worth copying:
  - Normalize at the adapter boundary only.
  - **Keep the vendor's own permission option ids through normalization**; a display label may not be a valid reply.
  - Declared capabilities must match what the vendor can actually do. Reject unsupported operations *before* touching the filesystem.
  - Separate the integration *kind* from the configured *instance* (account and config).
  - Never replay side effects after a provider process is lost.
- **Agent Client Protocol (ACP).** An open standard from Zed, protocol v1, with a Rust SDK, `agent-client-protocol` 3.3.0 ([repo](https://github.com/agentclientprotocol/agent-client-protocol)).
  - **44 agents** speak it ([list](https://agentclientprotocol.com/overview/agents)), including Gemini CLI, **Hermes Agent (Nous Research)**, OpenCode, Cursor, GitHub Copilot, Kimi, Qwen, Mistral Vibe, Goose and Pi (via pi-acp).
  - **Claude** connects through `claude-agent-acp`, and **Codex** through **`codex-acp`**. codex-acp is maintained at agentclientprotocol/codex-acp, runs `codex app-server` internally, and **supports ChatGPT-subscription login**. Grok is reachable over ACP per T3 Code.
  - Protocol surface:
    - Agent-side requests: `initialize`, `authenticate`, `session/new`, `session/load`, `session/prompt`, `session/cancel`, `session/set_mode`.
    - Updates: `session/update` (message, thought, `tool_call` and `tool_call_update` chunks; plan; mode; compaction).
    - Permissions: `session/request_permission`, with option kinds `allow_once`, `allow_always`, `reject_once`, `reject_always` ([tool calls](https://agentclientprotocol.com/protocol/tool-calls)).
    - Tool call vocabulary: `ToolKind` (`read`, `edit`, `delete`, `move`, `search`, `execute`, `think`, `fetch`, …) and tool content `content`, `diff{path, oldText, newText}` or `terminal`.
    - Client-side services: `fs/read_text_file`, `fs/write_text_file` and `terminal/*`. Through these, **the host performs the agent's file writes and commands**.
- **Codex reviewed this plan twice** (read-only, from the neighbouring pane). Its second review raised 8 issues, all applied below:
  - client services are not an enforcement boundary;
  - updates need a backend-wide stream with envelopes, because `session/load` replays history;
  - `session/cancel` has no acknowledgement, and a shared process can hold several sessions;
  - PR 2 can't preserve behavior with a capability-flag-only trait;
  - `allow_always` has no defined scope;
  - terminal authentication doesn't use `authenticate`;
  - interactions and tool content need payloads;
  - tests need raw-wire fixtures.

  Its first review agreed with the overall direction:
  - Use ACP as the default interface to external agents.
  - Keep Loop's own loop behind a separate provider interface.
  - Put a neutral event layer before the TUI and `--print`.
  - Don't fabricate native tool calls from external notifications.
  - Build a state machine (connection → session → turn → pending requests), not just an event translator.

### Decision
1. **One protocol for external agents: ACP.** A new vendor is a **config entry** (command, args, env), not code. Codex runs through `codex-acp`. A direct app-server driver is *not* built. If a Codex-only gap ever blocks users, it can be added later as a second implementation of the same trait. That's an escape hatch, not the plan.
2. **Two separate extension axes:**
   - **Model providers** (Loop runs the loop): the existing `loop-ai` provider layer plus a `CredentialProvider` for API keys and *officially supported* OAuth only.
   - **Agent backends** (an external agent runs the loop): ACP.
   Both feed **one canonical event model**, which is also the roadmap's `loop-protocol` and the future `--print` JSON stream.
3. **Loop advertises ACP `fs` and `terminal` client capabilities, but is honest about what that guarantees.**
   - `HostServices` governs only the requests an agent routes through it. Advertising these capabilities doesn't force an agent, its subprocesses or its MCP servers to use them. ACP permission requests are also optional for the agent.
   - Shell commands run through Loop's terminal can still write files directly, bypassing `write_text_file`'s version check.
   - **The real enforcement boundaries are therefore:**
     - **process isolation:** the agent runs in a git worktree, or in Loop's sandbox when one is configured;
     - **restricted MCP access:** the agent only gets the MCP servers Loop passes it.
   - With agent-managed execution, Loop gives **no enforcement guarantee**, and the UI says so ("execution managed by agent").
   - Shared-workspace changes are either serialized (one writing agent per tree) or isolated in worktrees.

---

## Architecture

```
 surfaces (TUI, --print, desktop later)      read only loop-protocol events
        │
 loop-protocol  SessionUpdate · ToolKind · ToolContent · Interaction · StopReason · ErrorInfo · Capabilities (versioned, serde)
        │
 AgentBackend (trait) ─────────────┬──────────────────────────────┐
   NativeBackend (AgentHarness)    AcpBackend (any ACP agent)      (future: direct drivers, same trait)
        │                                   │
 loop-ai ModelProvider + CredentialProvider  BackendSupervisor (process, JSON-RPC, restart, backpressure)
        │                                   │
        └──────────── HostServices (trait, implemented once by Loop) ┘
              permissions (policy + approval UI) · fs read/write (review, version checks) · terminal (ExecutionEnv/sandbox)
```

### Core types (`crates/loop-protocol`, new; the roadmap's crate)
```rust
pub const PROTOCOL_VERSION: u32 = 1;

pub struct UpdateEnvelope {                    // every update carries its identity, so routing never guesses
    instance: BackendInstanceId, generation: u64 /* process restart counter */,
    session: SessionKey, turn: Option<TurnId>, seq: u64,
    origin: Origin /* Live | Replay */,        // replayed history (session/load) is never re-appended
    update: SessionUpdate,
}
pub enum SessionUpdate {                       // tagged serde, snake_case; vocabulary mirrors ACP
    TurnStarted { turn_id },
    MessageStarted { message_id, role }, MessageEnded { message_id },   // explicit message boundaries
    MessageChunk { message_id, role: Role, content: ContentBlock },     // Role::{User, Agent}
    ThoughtChunk { content: ContentBlock },
    ToolCall(ToolCall),                        // id, title, kind: ToolKind, status, content, locations, raw_input
    ToolCallUpdate(ToolCallUpdate),            // id + changed fields only
    Plan(Vec<PlanEntry>),
    Usage(Usage),                              // optional per backend
    Notice { level, text },                    // reroutes, warnings, compaction
    InteractionRequested(InteractionRequest),
    InteractionResolved { id, outcome },
    TurnEnded { turn_id, stop: StopReason, error: Option<ErrorInfo> },
}
pub enum ToolKind { Read, Edit, Delete, Move, Search, Execute, Fetch, Think, Other }
pub enum ToolContent { Content(ContentBlock) /* text, image, resource */, Diff { path, old_text: Option<String>, new_text: String },
    Terminal { id }, Unknown(serde_json::Value) /* preserved, shown raw */ }
pub enum InteractionRequest {                  // payload-bearing; every variant keeps native ids verbatim
    Permission { id, tool_call: ToolCallUpdate, options: Vec<PermissionOption { native_id, label, kind_hint }> },
    Question   { id, prompt, choices: Option<Vec<Choice>> },
    Form       { id, schema: serde_json::Value },          // reply is a structured value
    Url        { id, url, purpose },
    Unsupported{ id, method: String, raw: serde_json::Value }, // answered with an explicit error, never silently dropped
}
pub enum InteractionOutcome { Selected { native_option_id }, Answered(serde_json::Value), Cancelled, Rejected { reason } }
// kind_hint (allow_once / allow_always / …) is a UI hint only. Loop returns the chosen native id and does NOT
// infer its own grant or suppress later requests, because ACP defines no scope or matching rule for "always".
pub struct ErrorInfo { category: ErrorCategory /* Auth | Quota | RateLimit | Network | Protocol | Agent | Cancelled | Internal */,
    retryable: bool, message: String }
pub struct Capabilities { load_session, set_model, modes, usage, fs_client, terminal_client, images, steering: bool }
```
Unknown wire variants deserialize to `Other` or are skipped, so newer agents never break Loop.

### Traits (`crates/loop-protocol/src/backend.rs`)
```rust
#[async_trait] pub trait AgentBackend: Send + Sync {
    fn id(&self) -> &BackendInstanceId;  fn capabilities(&self) -> &Capabilities;
    /// One backend-wide stream, subscribed BEFORE any RPC, so updates from session/load and from
    /// out-of-band notifications are never lost. Consumers route by the envelope's session key.
    fn subscribe(&self) -> broadcast::Receiver<UpdateEnvelope>;
    fn auth_methods(&self) -> &[AuthMethod];   // typed: Agent (authenticate RPC) | Terminal (run command interactively) | EnvVar
    async fn new_session(&self, spec: SessionSpec /* absolute cwd, mcp_servers, model, mode */) -> Result<SessionRef>;
    /// Replays history as Origin::Replay updates, then returns. Takes the same cwd and MCP spec as new_session.
    /// Uses session/resume where negotiated (no replay); Err(Unsupported) otherwise.
    async fn load_session(&self, r: &SessionRef, spec: SessionSpec) -> Result<SessionRef>;
    /// Starts a turn. The returned handle resolves to exactly one TurnOutcome, which the supervisor
    /// owns, so cancellation and process loss always end the turn deterministically.
    async fn prompt(&self, s: &SessionRef, input: Vec<ContentBlock>) -> Result<TurnHandle>;
    /// Fire-and-forget: ACP session/cancel is a notification with no acknowledgement. Returns once
    /// the notification is sent. The TurnHandle still resolves (Cancelled, or Uncertain after escalation).
    fn cancel(&self, s: &SessionRef);
    async fn shutdown(&self);
}
pub enum TurnOutcome { Completed(StopReason), Cancelled, Failed(ErrorInfo), Uncertain /* process lost mid-turn */ }

// Optional, typed service interfaces. A backend exposes the ones it supports. Callers ask
// `backend.service::<dyn T>()`, not a capability flag, so a missing operation can't be called by accident.
pub trait ModelControl  { async fn models(&self) -> Result<Vec<ModelInfo>>; async fn set_model(&self, s, m) -> Result<()>; async fn efforts(&self, s) -> Result<Vec<Effort>>; async fn set_effort(&self, s, lvl) -> Result<()>; }
pub trait ModeControl   { async fn set_mode(&self, s, mode) -> Result<()>; }
pub trait Compaction    { async fn compact(&self, s, instructions: Option<String>) -> Result<()>; }
pub trait Forking       { async fn fork_points(&self, s) -> Result<Vec<ForkPoint>>; async fn fork(&self, s, at) -> Result<SessionRef>; }
pub trait SessionStats  { fn stats(&self, s) -> SessionStatsSnapshot; }
pub trait LocalShell    { async fn run(&self, s, cmd) -> Result<ExecResult>; }   // `!bang`, native only
// Native-only areas (sandbox control, workflows, skills, MCP refresh) stay behind further typed services.
#[async_trait] pub trait HostServices: Send + Sync {       // Loop implements once; every backend calls it
    async fn interact(&self, req: InteractionRequest, cancel: CancellationToken) -> InteractionOutcome;
    async fn read_text_file(&self, s: &SessionRef, path, range) -> Result<String>;
    async fn write_text_file(&self, s: &SessionRef, path, content) -> Result<()>;   // policy + review + version check
    async fn terminal(&self, s: &SessionRef, req: TerminalRequest) -> Result<TerminalHandle>;
}
pub struct SessionRef { backend: BackendId, native_id: String, cwd: PathBuf, meta: serde_json::Value }
```

### Implementations
- **`NativeBackend`** (`crates/loop-agent`) wraps `AgentHarness` and maps `AgentEvent` to `SessionUpdate`:
  - built-in tools map to `ToolKind`: read → Read, write/edit → Edit, bash → Execute;
  - `mcp__*` tools map to Other with `raw_input`.
  - Its approval hooks call `HostServices::interact`. Loop's own tools and external agents then share one policy path, which is also the roadmap's P0 permission enforcement.
- **`AcpBackend`** (`crates/loop-acp`, new) is generic over any ACP agent and built on the `agent-client-protocol` crate. It maps ACP types to canonical ones almost one to one and routes ACP client requests to `HostServices`.
- **`BackendSupervisor`**, inside `loop-acp`, owns the connection → session → turn → pending-request state machine. It handles:
  - **Spawning** in its own process group (Unix) or Job Object (Windows), so killing it also kills descendants; `kill_on_drop` alone doesn't. It also sets an env allowlist (no `*_API_KEY` leakage, per CONTRIBUTING invariant 5) and an absolute cwd.
  - **Draining** stderr to the log, and using bounded queues.
  - **Owning every in-flight `session/prompt` future**, so each turn produces exactly one `TurnOutcome`.
  - **Crash restart.** It bumps `generation` and marks **every** session with a live turn as `Uncertain`, because one process may host several sessions. Answers and updates from an older generation are discarded.
  - **Cancel escalation.** It sends `session/cancel` and answers pending interactions with `cancelled`. If no prompt response arrives before a timeout, it kills the process tree, which escalates as above.

### Registry: subscriptions are providers
To the user, an external agent is a **subscription provider**. It is not a separate mode to switch into: it's signed in with `/login`, next to the API-key providers, and its models appear in `/model` as `<subscription>/<model>`. Choosing one of its models starts its ACP agent and routes prompts there; choosing any other model switches back to Loop's own agent. This follows prime-agent, which lists "ChatGPT Plus/Pro (Codex Subscription) · subscription" among its providers.

**Each subscription is data** (`AgentEntry` in settings `agents`, global only):
- command, args, env;
- `label` for `/login`;
- `authMethod`: the agent-handled ACP sign-in method, for ChatGPT `chat-gpt`. The `api-key` method is never used, because OpenAI API keys stay the separate "OpenAI · api key" provider;
- `homeEnv`: for Codex `CODEX_HOME`, which Loop points at `<agent dir>/agents/<id>/home`, so a standalone Codex sign-in is never reused.

**Built-in:** `chatgpt` (`npx -y @agentclientprotocol/codex-acp`).

**Sign-in, models and sign-out:**
- Sign-in runs ACP `authenticate` with the declared method, then reads the models from a session's `model` config option and caches them for `/model`.
- `ModelControl` switches models with `session/set_config_option`.
- `ModelControl` also sets the reasoning effort from the session's `thought_level` config option (for Codex: low … ultra). `/effort [level]` and Shift+Tab use it for subscriptions and Loop's own models alike; a subscription's last effort is saved in `<agent dir>/agents/<id>/effort` and applied to new sessions when the model offers it.
- `/logout <id>` deletes the private home, the marker and the model cache.

### Sessions
- A Loop session records the backend binding `{backend, native_id, meta}` in a `Custom` entry (`crates/loop-agent/src/session/types.rs:123`; a `PendingSessionWrite::Custom` variant needs adding). It also mirrors the canonical updates for display and `/session`.
- Resume calls `load_session` when the agent supports it. Otherwise it starts a new external session seeded with a handoff summary, and the UI says so.
- Mirrored history is never fed back into a model.

---

## Edge cases (combined from Codex's review, T3 Code's notes and the ACP spec)
| Area | Handling |
|---|---|
| Binary or adapter missing; `npx` offline | Typed `ErrorInfo{Agent}` with install hint from the preset. Show it in `/login`; don't crash. |
| Protocol and version skew | Negotiate `protocolVersion` in `initialize`. Pin a minimum adapter version per preset. Unknown updates are skipped and logged. Enable experimental features only deliberately. |
| Auth | Choose the flow by the advertised auth-method type. **Agent auth:** wait for `authenticate` to return. **Terminal auth:** run the configured command interactively (no `authenticate` call), check its exit status, then reconnect and `initialize` again. **Env var:** prompt for the value and pass it to the child process only. A mid-session auth error ends the turn as `Auth` and sends the user to `/login`. Loop never stores vendor tokens; the agent's own CLI does. |
| Rate limit vs exhausted plan | `RateLimit` (retryable, bounded backoff) vs `Quota` (not retryable, show reset info if given). Token usage is not remaining allowance. |
| Agent crash mid-turn | Every session with a live turn in that process becomes `TurnOutcome::Uncertain`. Bump `generation`, restart, re-`initialize`, then `load_session` if supported. **Never replay the prompt**: edits or commands may already have run. |
| Cancel with prompts pending | `cancel` is fire-and-forget (ACP has no acknowledgement). Answer every pending interaction with `cancelled`, then wait for the `session/prompt` response, which ends the turn. On timeout, kill the process tree; that affects every session in it, which are marked uncertain. Drop late answers using generation/session/turn/request ids. |
| Replay on load | Updates from `session/load` arrive as `Origin::Replay`. They rebuild the transcript and are never appended to Loop's store, so history isn't duplicated. Use `session/resume` (no replay) when the agent supports it. |
| Unknown or unsupported requests from the agent | Answer with an explicit JSON-RPC error. Never ignore them, or the agent hangs. Unknown update payloads are kept as `Unknown` and shown raw. |
| Concurrent requests | One prompt at a time per session (queue in the surface). Several sessions per process are fine. Several permission requests are queued in the UI and answered by id. |
| Permission semantics | Return the selected native `optionId`. `allow_always` is only a UI hint: ACP defines no scope or matching rule for it, so Loop doesn't create a grant or suppress later requests from it. Questions, forms and URL flows are separate payload-bearing variants; a yes/no prompt isn't enough. |
| Sandbox and cwd | Always send an absolute cwd. With `fs`/`terminal` capabilities, Loop's `ExecutionEnv` and sandbox apply. Otherwise the agent's own sandbox applies, and the status bar says "execution managed by agent". |
| Several agents on one tree | `write_text_file` checks a per-file version hash from this session's last read ("file changed since you read it"). Reject-revert only if the file still matches what this session wrote; otherwise report a conflict. These checks don't cover shell commands, so a tree with more than one writing agent is either serialized or split into worktrees. |
| Large output and stray stdout | Drain continuously into bounded queues, stash large content to disk, strip terminal escape codes. Non-JSON lines on stdout are logged and skipped. |
| Windows | Resolve `npx.cmd` and `.exe`, use process-group kill, normalize paths. Native Windows and WSL are tested as separate environments. |
| Model reroutes and compaction | Shown as `Notice`. Compaction is the agent's own, so Loop's `/compact` reports "unsupported" for external backends. |
| Secrets | Child process env is built from an allowlist and the preset env. No API keys from Loop's credential store unless the preset asks for them. |

---

## PR sequence
1. **`loop-protocol` crate**
   - Canonical types (above), serde with a version tag.
   - Snapshot tests (`insta`) of the JSON form.
   - `NativeBackend`'s `AgentEvent` → `SessionUpdate` mapper, with tests using the faux provider.
   - No surface changes yet.
2. **`AgentBackend` + `HostServices`, surfaces on canonical events.** Split into small steps that can each land without breaking anything:
   - **2a. Inventory and parity tests.** List every `AgentHarness` call made by `Runtime`, the TUI and print mode: prompting, models, thinking level, compaction, fork, stats, `!bang`, sandbox, skills, MCP, shutdown, approval hooks. Add parity tests that record today's behavior through the faux provider: event order, final message, persisted entries.
   - **2b. Typed services with native implementations.** Add `AgentBackend` plus the optional services (`ModelControl`, `Compaction`, `Forking`, `SessionStats`, `LocalShell`, …), all implemented by `NativeBackend` by delegating. Move callers over one area at a time; parity tests stay green.
   - **2c. Surfaces on `SessionUpdate`.** Rewrite the TUI's `handle_agent_event` (`crates/loop-cli/src/app.rs:3191`), transcript rebuild (`tui/mod.rs:1666`) and print mode (`print_mode.rs`). Move the `ToolApprovalBridge` logic (`crates/loop-app-core/src/tool_approval.rs`) behind `HostServices::interact`. Only then does `Runtime.harness` (`crates/loop-app-core/src/runtime.rs:38`) become `Arc<dyn AgentBackend>`.
   - Desktop keeps using `AgentHarness` through `NativeBackend` for now.
3. **`loop-acp` crate**
   - `AcpBackend` + `BackendSupervisor` on `agent-client-protocol` 3.3.
   - Client-side `fs` and `terminal` handlers go to `HostServices`, using Loop's `ExecutionEnv` and review flow.
   - **Hermetic tests at two levels:**
     - A scripted fake ACP agent built with the same crate's agent side. It covers text, a tool call with a diff, permission round trips (all four option kinds) and an auth error.
     - **Raw-wire JSONL fixtures**, written without the SDK, so the crate can't hide its own compatibility assumptions. They cover:
       - `session/load` replay;
       - frames split across reads;
       - unknown requests (which must get an error reply) and unknown updates;
       - a slow UI and backpressure;
       - the terminal lifecycle (output, wait, kill, release);
       - cancel with a pending permission;
       - a crash with two live sessions in one process (both become `Uncertain`);
       - stale answers from an older generation.
   - Adapter versions for the presets are pinned and checked manually outside CI.
4. **Registry, `/login`, `/model`, resume**
   - `agents` settings plus built-in presets (codex, claude, gemini, hermes, opencode).
   - Generic `/login` from `authMethods`; `/model` lists `<agent>/<model>`.
   - Session binding and `load_session` resume.
   - Switching backends starts a new Loop session.

## Verification
- **CI (hermetic):** fake-agent integration tests, protocol snapshot tests, and the existing suites unchanged. No real vendor binaries in CI.
- **Manual, on your ChatGPT plan:**
  - `/login` → Codex → ChatGPT sign-in completes, and `/model` lists `codex/*`.
  - A chat answer streams.
  - "run ls" shows a permission prompt with Codex's native options. Force an approval-requiring mode for this test, so the prompt actually appears.
  - "create hello.txt" shows the diff before or with approval.
  - Esc cancels cleanly, even with a prompt pending.
  - `kill` the `codex-acp` process mid-turn: the turn is marked uncertain, the agent restarts, and the prompt isn't re-sent.
  - `loop --resume <id>` continues the thread.
  - `loop --model codex/<m> --print "…"` gives the right output and exit code.
- **Uniformity check:** repeat the smoke test with `gemini --acp` or Hermes **with no code changes**. That's the acceptance test for "no per-vendor code".

## Immediately after approval
1. Save this plan in the repo as `docs/design/external-agents.md`. Don't commit it until you say so.
2. Build an interactive visualiser page that explains the architecture:
   - the layer diagram;
   - one turn flowing through the backend, supervisor and host services;
   - the cancel and crash state machine;
   - how adding a vendor works as a config entry;
   - the PR sequence.

## After this (roadmap P0, unchanged order)
1. **Permission enforcement.** Largely shared with `HostServices`. Cover headless `--print`, MCP serve and workers.
2. **Session records.** Write each message as it ends, and keep records on cancel.
3. **Real compaction.** Model-written summary, automatic triggering.
4. **`--print` JSON/stream-json and exit codes.** The `loop-protocol` stream is already the schema.

## Parked CI backlog (only when it blocks something)
Release hardening (desktop failure shouldn't block a CLI release, preflight, remove certificate debug output, release caches `save-if: false`) · merge queue, DCO and PR template · desktop CI and the `apps/desktop` move · cargo-deny, Dependabot and MSRV · xtask.
