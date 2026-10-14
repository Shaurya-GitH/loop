# AgentHarness call inventory

Every place outside `loop-agent` that calls `AgentHarness`, grouped by the
`AgentBackend` interface or typed service it moves to. This is step 2a of
[External agents](external-agents.md). Steps 2b and 2c move these callers one
group at a time while `crates/loop-agent/tests/harness_parity.rs` stays green.

Line numbers are as of this commit.

## AgentBackend (every backend)

| Call | Call sites | Becomes |
|---|---|---|
| `prompt` | `loop-cli/src/app.rs:1574`, `loop-cli/src/print_mode.rs:153`, `loop-desktop/src/controller.rs:602,639` | `prompt` → `TurnHandle` |
| `abort` | `loop-cli/src/app.rs:2341`, `loop-desktop/src/controller.rs:792` | `cancel` (fire-and-forget) |
| `phase` | `loop-cli/src/app.rs:1496,1598`, `loop-desktop/src/controller.rs:568,645,946` | turn in progress, from the update stream |
| `subscribe` | `loop-cli/src/app.rs:332`, `loop-desktop/src/controller.rs:270` | `subscribe` → `UpdateEnvelope` stream |
| `wait_for_idle` | `loop-desktop/src/controller.rs:604,797` | await the `TurnHandle` |
| `request_shutdown` | `loop-cli/src/app.rs:958` | `shutdown` |
| `session_context` | `loop-cli/src/app.rs:392,3148` | transcript rebuild from stored updates (2c) |
| `start_new_session` | `loop-cli/src/app.rs:3842`, `loop-desktop/src/controller.rs:535` | `new_session` |
| `switch_session` | `loop-desktop/src/controller.rs:506` | `load_session` |

## Typed services (backend opts in)

| Service | Calls | Call sites |
|---|---|---|
| `ModelControl` | `set_model`, `clear_model`, `set_thinking_level` | `loop-app-core/src/runtime.rs:277,294`, `loop-cli/src/app.rs:4478`, `loop-desktop/src/controller.rs:390,415` |
| `Compaction` | `compact` | `loop-cli/src/app.rs:3971` |
| `Forking` | `fork_points`, `fork_session` | `loop-cli/src/app.rs:3098,3142` |
| `SessionStats` | `session_stats` | `loop-cli/src/app.rs:202,3996`, `loop-desktop/src/state/stats.rs:32` |
| `LocalShell` | `tool_env` for `!bang` commands | `loop-cli/src/app.rs:1686`, `loop-desktop/src/controller.rs:716` |

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

## Approvals (move behind `HostServices`, step 2c)

| Calls | Call sites | Notes |
|---|---|---|
| `set_before_tool_call`, `set_after_tool_call` | `loop-cli/src/app.rs:386,389`, `loop-desktop/src/controller.rs:183,186` | `ToolApprovalBridge` hooks become `HostServices::interact` |
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
