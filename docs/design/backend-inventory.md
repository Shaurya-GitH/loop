# AgentHarness call inventory

Every place outside `loop-agent` that calls `AgentHarness`, grouped by the
`AgentBackend` interface or typed service it moves to. This is step 2a of
[External agents](external-agents.md). Steps 2b and 2c move these callers one
group at a time while `crates/loop-agent/tests/harness_parity.rs` stays green.

Line numbers are as of this commit.

## AgentBackend (every backend)

| Call | Call sites | Becomes | Status |
|---|---|---|---|
| `prompt` | `loop-cli/src/app.rs:1574`, `loop-cli/src/print_mode.rs:153`, `loop-desktop/src/controller.rs:602,639` | `prompt` → `TurnHandle` | TUI and `--print` moved in 2c |
| `abort` | `loop-cli/src/app.rs:2341`, `loop-desktop/src/controller.rs:792` | `cancel` (fire-and-forget) | TUI moved in 2c |
| `phase` | `loop-cli/src/app.rs:1496,1598`, `loop-desktop/src/controller.rs:568,645,946` | `is_busy` | TUI moved in 2c |
| `subscribe` | `loop-cli/src/app.rs:332`, `loop-desktop/src/controller.rs:270` | `subscribe` → `UpdateEnvelope` stream | TUI moved in 2c |
| `wait_for_idle` | `loop-desktop/src/controller.rs:604,797` | await the `TurnHandle` | desktop later |
| `request_shutdown` | `loop-cli/src/app.rs:958` | `shutdown` | stays native: it runs on the native harness at exit |
| `session_context` | `loop-cli/src/app.rs:392,3148` | `transcript` → updates replayed through the live renderer | TUI moved in 2c |
| `start_new_session` | `loop-cli/src/app.rs:3842`, `loop-desktop/src/controller.rs:535` | `new_session` | TUI moved in 2b |
| `switch_session` | `loop-desktop/src/controller.rs:506` | `load_session` | desktop later |

## Typed services (backend opts in)

| Service | Calls | Call sites | Status |
|---|---|---|---|
| `Compaction` | `compact` | `loop-cli/src/app.rs:3971` | TUI moved in 2b |
| `SessionStats` | `session_stats` | `loop-cli/src/app.rs:202,3996`, `loop-desktop/src/state/stats.rs:32` | TUI moved in 2b |
| `LocalShell` | `tool_env` for `!bang` commands | `loop-cli/src/app.rs:1686`, `loop-desktop/src/controller.rs:716` | TUI moved in 2b |
| `ModelControl` | `set_model`, `clear_model`, `set_thinking_level` | `loop-app-core/src/runtime.rs:277,294`, `loop-cli/src/app.rs:4478`, `loop-desktop/src/controller.rs:390,415` | Defined with external agents (PR 4). Loop's own model selection keeps going through `Runtime::select_model`, which also checks credentials. |

`/new` now goes through `AgentBackend::new_session` (2b). Desktop keeps
calling the harness until it moves to the backend interface.

## Native-only (stay on `NativeBackend`)

These configure Loop's own agent loop and have no meaning for an external
agent. Callers reach them through the native backend's own services.

| Area | Calls | Call sites |
|---|---|---|
| Provider tuning | `set_response_header_timeout_ms`, `response_header_timeout_ms` | `loop-cli/src/app.rs:3932,3948` |
| Sandbox | `sandbox_info`, `set_sandbox`, `clear_sandbox`, `set_tools` | `loop-cli/src/app.rs:3720,3740,3782,3787` |
| Skills and resources | `activate_skill`, `clear_active_skills`, `set_resources` | `loop-cli/src/app.rs:4268`, `loop-app-core/src/runtime.rs:91,109,113` |
| Tools and MCP | `get_tools`, `tools_snapshot` | `loop-app-core/src/runtime.rs:790`, `loop-cli/src/mcp_serve.rs:87` |
| JSON hooks | `on` | `loop-app-core/src/hooks_load.rs:64` |
| Workflows | `start_workflow_from_goal` | `loop-cli/src/app.rs:4394` |
| Forking | `fork_points`, `fork_session` | `loop-cli/src/app.rs:3098,3142` |

Forking works on Loop's own session tree, so it stays native. After a fork
the TUI calls `NativeBackend::adopt_session` so updates carry the new
session key.

## Approvals (behind `HostServices`, step 2c)

The TUI builds `ToolApprovalBridge::with_host(InteractionQueue)`: Loop's own
approvals become `InteractionRequest::Permission`s with the native options
`accept`, `accept_session` and `reject`, and the TUI's picker answers any
backend's permission request through `ApprovalReply`, which maps its three
choices onto the request's options by kind. Desktop keeps the original
`ApprovalPrompt` channel (`ToolApprovalBridge::new`) for now.

| Calls | Call sites | Notes |
|---|---|---|
| `set_before_tool_call`, `set_after_tool_call` | `loop-cli/src/app.rs:386,389`, `loop-desktop/src/controller.rs:183,186` | still install the bridge's hooks on the native harness |
| `tool_env` | `loop-cli/src/app.rs:357`, `loop-desktop/src/controller.rs:167` | approval bridge reverts rejected edits through it |
| `read_session_entries`, `append_label` | `loop-cli/src/app.rs:358,378` | "accept all" grants stored as session labels |

## Behavior pinned by the parity tests

`crates/loop-agent/tests/harness_parity.rs`:

- A text turn emits `agent_start, turn_start, message_start/end (prompt),
  message_start, message_update…, message_end, turn_end, agent_end` and
  persists `[user, assistant]`.
- A tool turn runs the tool between `tool_execution_start` and
  `tool_execution_end`, has two loop turns, and persists
  `[user, assistant, toolResult, assistant]`.
- A provider error comes back as an assistant message with
  `stop_reason = error`. `prompt` does not return `Err`.
- A blocking `before_tool_call` hook ends the call as an error that carries
  the reason, and the tool does not run.
- `abort` during a tool ends the turn, and nothing from that turn is
  persisted. Changing that belongs to the session-records work.

Known gap found while writing these: after a tool is cancelled, the loop
still starts its next model call. Real providers abort it through the
cancellation token, so the turn still ends as aborted, but the request is
sent. Tracked for the session-records work.
