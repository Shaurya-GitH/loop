# P0-3 + P0-4 · Tool gate, hooks and permission modes: design draft

Status: accepted; PR 1 (the gate) implemented. Based on `origin/main` at cb4a0c1 (v0.4.0).

Goal: every tool call goes through one gate, in every mode. Loop's own tools,
MCP tools and an external agent's permission requests all pass the same chain:

```
deny rules  →  hooks  →  permission mode  →  ask the user (only if the mode says "ask")
```

- P0-3 builds the gate and the hooks.
- P0-4 adds the four permission modes as the gate's last step.

## 1. What exists today

| Piece | Where | State |
|---|---|---|
| Lifecycle hooks | `loop-agent/src/harness/hooks.rs` (`HarnessHookEvent`, `HookRegistry`) | Turn-start, compact, tree, settled, queue, shutdown and workflow events. The outcome is `{cancel, summary}`. There are no tool events. |
| JSON hook files | `loop-app-core/src/hooks_load.rs` | `on`, `match`, `action{block,reason,summary}`. `match` is parsed but never used. `on: "tool_call"` is silently ignored. A second loader, `load_hooks_into_registry`, drops `summary` and skips invalid files without a word. |
| Discovery | `resources/mod.rs:157` | `~/.loop/agent/hooks/*.json`, plus `<project>/.loop/hooks/*.json` when the project is trusted. |
| Tool slots | `Agent::set_before_tool_call` / `set_after_tool_call` (`agent.rs:177`), `AgentHarness::set_*` (`agent_harness.rs:589`), called at `agent_loop.rs:894` and `:1010` | One slot each. *Before* can block with a reason; *after* patches the result. |
| Approval bridge | `tool_approval.rs` (`ToolApprovalBridge`) | Fills both slots **only in the TUI** (`app.rs:410`) and desktop (`controller.rs:185`). Applies `toolPermissions` (allow/ask/deny per tool), `fileEditReview` (`newSession`/`always`/`never`, the `ApprovalPolicy` enum), session "accept all" grants per group (file, bash, web), and the file-edit diff review. |
| Defaults | `default_tool_permissions` (`tool_approval.rs:104`) | write/edit/bash/web_fetch/web_search: `ask`; read: `allow`. |
| Rhai extensions | `extensions/mod.rs:53` | `on(event)` is a no-op stub. |

### Gaps this design closes
1. **`--print` has no gate.** `toolPermissions: deny` isn't enforced there today.
2. **`mcp serve` skips the agent loop.** `LoopToolProvider::new(harness.tools_snapshot())` (`mcp_serve.rs:87`) calls `tool.execute` directly.
3. **`/workflow` tasks skip every check.** Shell-command tasks (`ShellWorker`) run `host_env.exec` directly, without even the `bash` blocklist. Agent-turn tasks (`AgentWorker`) run their own agent loop without the harness's before/after hooks, so their tool calls skip approval even in the TUI.
4. **ACP agents** run their own tools. Loop sees only `session/update` tool calls and `session/request_permission` (`loop-acp/src/backend.rs:699`, `ask_permission` at 826).
5. **A project can loosen permissions.** The settings overlay (`config/settings.rs:390`) takes `fileEditReview` from the project unconditionally, so a project file without the key resets the global value. It also merges the project's `toolPermissions` over the global ones, so a trusted repo can set `bash: allow`.

---

## Part A · P0-3: the gate and hooks

### A.1 One gate in `loop-app-core`, installed by bootstrap
New module `tool_gate.rs` with `ToolGate`:

```
before:  deny rules → hooks (tool_call) → mode policy → approver
after:   file-edit review (TUI/desktop) → hooks (tool_result)
```

- **Installation:** `bootstrap` installs the gate in the harness slots in every mode. The TUI and desktop no longer call `set_before_tool_call`; they attach their prompt with `gate.set_approver(bridge)`. `loop-agent` keeps its single slot unchanged.
- **`mcp serve`:** `LoopToolProvider` takes the gate and runs `before → execute → after` around each call. Wrapping each tool's `execute` instead would run the gate twice inside the agent loop.
- **Workflow `ShellCommand`:** these tasks go through the gate as a synthetic `bash` call (`kind: execute`). They can follow in a later PR if the scheduler change grows.
- **Until P0-4 lands,** the "mode policy" step reproduces today's behaviour: `toolPermissions` plus `fileEditReview`.

### A.2 Events
New `HarnessHookEvent` variants, emitted by the gate:

```rust
ToolCall {
    source: ToolSource,        // Native | Mcp { server } | Agent { id }   e.g. Agent { id: "chatgpt" }
    tool: String,              // "bash", MCP tool name, or the ACP title
    kind: ToolKind,            // read | edit | delete | move | search | execute | fetch | other
    args: serde_json::Value,   // validated args; ACP: rawInput when present
    call_id: String,
    session_id: String,
    cwd: PathBuf,
    mode: PermissionMode,      // the active mode (P0-4)
},
ToolResult { /* same identity fields */ is_error: bool, output: String /* capped */ },
```

- **`kind`** comes from `protocol_events::tool_kind` for native tools and `loop-acp` `mapping::tool_kind` for ACP. A rule `"kind": "execute"` then covers both Loop's `bash` and Codex's exec.
- **Hook outcome:** `HookOutcome` gains `decision: Option<ToolDecision>`, where `ToolDecision` is `Allow` or `Block { reason }`.
  - Across handlers: any `Block` wins, then `Allow`, then none.
- **Not in v1:** rewriting arguments. Rewritten arguments would skip validation and wouldn't match what the prompt showed.
- **`tool_result`:** observe only, plus an optional `appendContext` (capped) added to what the model sees.

### A.3 Precedence (fixed, used by every mode)
1. **Deny rule** → block. Nothing overrides it.
2. **Hook `Block`** → block, with the hook's reason sent to the model.
3. **Hook `Allow`** → allowed, with no prompt for this one call. It never creates a permanent grant.
4. **Allow rule** → allowed.
5. **Permission mode** (Part B) → allow, ask or deny.
6. **"Ask" with nobody to ask** (`--print`, `mcp serve`, workers) → deny, with a reason that names the flag or rule that would allow it.

Hooks and deny rules apply in **every** mode, `full-access` included, because the user wrote them on purpose.

### A.4 Hook files (JSON)
```json
{
  "name": "no-force-push",
  "on": "tool_call",
  "match": { "kind": "execute", "tool": "bash|exec*", "args": { "command": "*git push*--force*" } },
  "action": { "block": true, "reason": "Force push is not allowed here" }
}
```
- **`match`:** every field is optional, and all the fields given must match.
  - `tool`: glob, with `|` for alternatives.
  - `kind`: a tool kind.
  - `source`: `native`, `mcp`, `mcp:<server>`, `agent` or `agent:<id>`.
  - `mode`: a mode name, or a list of names.
  - `args`: an object of key or JSON pointer → glob on the string value.
  - The old `argsContains` stays as a substring match on the serialized args, with a deprecation warning.
- **`action`:** `block` + `reason`, `allow: true`, `summary` (compact/tree, unchanged), or `command` (A.5).
- **One loader:** `register_json_hooks`; delete `load_hooks_into_registry`. Invalid files show a TUI notice or a `--print` stderr line.
- **Compatibility warning:** a file with `on: "tool_call"`, `block: true` and no `match` is ignored today but would now block every tool. The loader warns about such files, and the release notes say so.

### A.5 Command hooks
```json
{ "on": "tool_call", "match": { "kind": "edit" },
  "action": { "command": ["python3", "~/.loop/agent/hooks/check_edit.py"], "timeoutMs": 10000, "onError": "block" } }
```
- **`command`:** an argv array, with `~` expanded. It's never a shell string, so hooks behave the same on Windows.
  - Runs with the session's cwd.
  - Environment: `LOOP_HOOK_EVENT`, `LOOP_SESSION_ID`, `LOOP_PROJECT_DIR`, `LOOP_PERMISSION_MODE`.
- **stdin:** the event as JSON. Tool output is capped at 64 KB.
- **Exit codes:**
  - `0` with empty stdout: no decision.
  - `0` with JSON stdout `{"decision":"allow"|"block","reason":…,"appendContext":…}`: use that decision.
  - `2`: block, with the first 4 KB of stderr as the reason.
  - Anything else: an error.
- **stdout:** capped at 64 KB. Output that isn't JSON counts as an error.
- **Errors, timeouts and crashes:** `onError: "block"` blocks the call; `"ignore"` lets it through and shows a warning.
  - Default for `tool_call`: `block`. Default for every other event: `ignore`.
  - `timeoutMs`: default 10000, maximum 120000.
- **Cancellation:** the hook runs in its own process group (Unix) or job object (Windows). It's killed on timeout or when the turn is cancelled.
- **Concurrency:** hooks run outside the approval prompt's lock, so parallel tool calls run their hooks in parallel. Only the user prompt is serialized.

### A.6 External agents (ACP)
- **`session/request_permission`:** in `ask_permission`, build a `ToolCall` event from the request's tool call (title, kind, rawInput) with `source: Agent { id }`. Run the gate before anything is published to the host. `loop-acp` reaches the gate through a new optional `HostServices::review_permission(&ToolCallInfo) -> Option<ToolDecision>`, which app-core implements, so `loop-acp` doesn't depend on app-core.

  | Gate result | Answer to the agent |
  |---|---|
  | block | `reject_once`; if there's none, `reject_always`; otherwise `cancelled` |
  | allow | `allow_once`, never `allow_always`; if there's no `allow_once`, ask the user |
  | ask | today's prompt |
- **`session/update` tool calls:** sent to hooks as observe-only `ToolCall` / `ToolResult` events. The agent runs these itself, so a decision there is ignored (logged in debug).

### A.7 Not in scope
- **Rhai `on()` stays a stub.** Later it registers into the same `HookRegistry`.
- **Trust prompt:** project hooks still load only for trusted projects. The prompt text changes to say that trusting a project lets it run commands through hooks.

---

## Part B · P0-4: permission modes

### B.1 The four modes

| Mode | read, search | web_search, web_fetch | write / edit | bash | MCP tools |
|---|---|---|---|---|---|
| `read-only` | allow | allow | **deny** | **deny** | allow if `readOnlyHint`, otherwise **deny** |
| `ask` (default) | allow | ask | ask | ask | ask (allow if `readOnlyHint`) |
| `accept-edits` | allow | allow | allow inside the workspace; ask outside it or for `.git/`, `.loop/`, `.env*` | ask | ask (allow if `readOnlyHint`) |
| `full-access` | allow | allow | allow | allow | allow |

- **Workspace:** the session cwd, or the git root if there is one. Paths are canonicalised first, so `../` and symlinks out of the workspace count as "outside".
- **`readOnlyHint`:** the MCP tool annotation. An MCP tool without it is treated as able to change things.
- **`ask` mode matches today's defaults**, including web tools asking (#59). Upgrading changes nothing for users who don't pick a mode.
- **"Accept all for this session"** in the prompt keeps working as today (per group: file, bash, web), and only lasts until the mode changes.

### B.2 Rules under the modes
- **Settings shape:** `toolPermissions` stays as the per-tool rule map, extended with patterns:
  ```json
  "toolPermissions": {
    "bash": "ask",
    "bash(git status*)": "allow",
    "bash(rm -rf *)": "deny",
    "write(~/secrets/**)": "deny",
    "mcp:github/*": "ask"
  }
  ```
- **Patterns:**
  - Bash patterns match each part of a command, after splitting on `&&`, `||`, `;`, `|` and `$(…)`. An allow rule applies only when **every** part matches an allow rule. A command that can't be parsed never matches an allow rule.
  - Path patterns are globs on the canonical path.
- **Precedence** is A.3: deny rules, then hooks, then allow rules, then the mode.
- **`fileEditReview`** keeps only its display role: show the diff before or after the edit. `never` stops meaning "never ask". On first load, `fileEditReview: never` is migrated to `permissionMode: accept-edits`, with a one-time notice.

### B.3 Choosing the mode
- **Setting:** `permissionMode` in `~/.loop/agent/settings.json`. The default is `ask`.
- **For one run:** `--permission-mode <read-only|ask|accept-edits|full-access>`.
- **In the TUI:** `/permissions`, a picker with a one-line description per mode. `/permissions <mode>` switches directly. There's no key cycle, because Shift+Tab already sets effort.
- **Always visible:** the footer shows the mode, e.g. `ask`, `accept edits`, `read-only`.
- **Projects can only tighten.** `.loop/settings.json` may set a stricter `permissionMode` or add `deny`/`ask` rules. It can't set `allow` rules or a looser mode; those entries are ignored with a notice. The same applies to the `fileEditReview` overlay (gap 5).

### B.4 `full-access` safeguards
- **Confirmation on switching:** "Full access: every tool call runs without asking, including shell commands and file deletion. Deny rules and hooks still apply. Continue?"
- **Persistent banner:** a red footer banner for as long as the mode is on: `⚠ full access · tools run without approval`.
- **Startup:** starting in this mode (setting or flag) prints the same warning once.
- **Headless:** `--print`, `mcp serve` and workers get `full-access` only from the explicit `--permission-mode full-access` flag, never from settings alone. A setting is ignored there with a stderr note.
- **`mcp serve` warning:** it also warns when combined with `full-access` and no `--token`.

### B.5 Headless behaviour
- With nobody to ask, "ask" becomes deny (A.3 step 6). The model gets a reason such as: "blocked in ask mode: `bash` needs approval; rerun with --permission-mode accept-edits/full-access or add a toolPermissions allow rule".
- Running unattended therefore needs `--permission-mode full-access`, or allow rules.

### B.6 External agents: Loop's mode sets Codex's mode

| Loop mode | Codex `mode` Loop sets | What Loop does with Codex's permission requests |
|---|---|---|
| `read-only` | `read-only` | Rejects edit and command escalations; allows read-only ones. |
| `ask` | `read-only` | Codex asks before every edit and network use, and Loop asks the user. |
| `accept-edits` | `workspace-write` | Codex asks only to leave its sandbox. Loop allows edits inside the workspace and asks for the rest. |
| `full-access` | `agent-full-access` | Approves everything; the banner applies. |

- **When it's set:** at session start and whenever the mode changes, using the same select-option path as model and effort (a new `mode` category in `Setting`, `loop-acp/src/backend.rs`).
- **Codex's limit, shown in the UI** ("commands run by the agent"):
  - in `read-only`, Codex runs commands it considers read-only without asking;
  - in `workspace-write`, it runs sandboxed commands without asking;
  - Loop sees those only as tool-call updates, after the fact.
- **Agents without a mode option:** Loop applies its mode to their permission requests only, and the footer says "agent-managed".

### B.7 Module layout
- `loop-app-core/src/permissions/`
  - `mode.rs`: `PermissionMode`, parse/label, migration.
  - `rules.rs`: pattern parsing and matching, bash command splitting.
  - `policy.rs`: `fn decide(mode, rules, call) -> Allow | Ask | Deny { reason }`, a pure function with table tests.
- `ToolGate` calls `policy::decide` as its mode step. `ToolApprovalBridge` becomes the approver only: it renders the prompt and stores session grants.

---

## 3. Work plan (PR-sized)
1. **Gate skeleton (P0-3).** `ToolGate` in every mode, with today's policy. This fixes deny in `--print`.
2. **Tool events and matching.** One loader, notices, the compatibility warning.
3. **Command hooks.**
4. **`mcp serve` through the gate.**
5. **ACP permission requests through the gate.** Plus observe-only update events.
6. **Modes (P0-4).** `permissions/` module, `permissionMode`, `--permission-mode`, `/permissions`, footer, headless deny, `fileEditReview` migration.
7. **Rules and project tightening.** Pattern rules, bash splitting, the project-overlay fix (gap 5).
8. **`full-access` safeguards and the Codex mode mapping.**
9. **Docs.** `docs/hooks.md`, `docs/permissions.md`, the configuration table, trust prompt text, release notes.

## 4. Tests
- **Policy table:** mode × tool × path or command → allow, ask or deny.
  - Includes inside vs outside the workspace, symlink escape, `.git/` writes, and MCP with and without `readOnlyHint`.
- **Precedence:**
  - deny rule beats a hook's `allow` and `full-access`;
  - a hook's `Block` applies in `full-access`;
  - a hook's `Allow` skips the prompt;
  - an allow rule needs every part of a bash command to match (`git status && rm -rf x` still asks).
- **Headless:**
  - `--print` in `ask` refuses `bash` with the reason;
  - `full-access` from settings is ignored in `--print`, and the flag works;
  - `mcp serve`: an MCP client's `bash` is blocked by a hook and by `read-only`.
- **Projects:** `.loop/settings.json` with `bash: allow` or `permissionMode: full-access` is ignored with a notice; a stricter mode applies.
- **Hooks:** matching table, deprecated `argsContains`, warning for a `tool_call` block with no `match`.
- **Command hooks:**
  - each exit-code case; malformed stdout; timeout; crash; `onError` both ways; capped stdout;
  - cancelling the turn kills the child;
  - argv-only on Unix and Windows.
- **ACP (fake agent):**
  - a hook `Block` answers `reject_once` without asking the host;
  - `Allow` answers `allow_once`;
  - the fallback when an option is missing;
  - a mode change sends the `mode` option;
  - update events reach a hook that only observes.
- **Live (Herdr test pane):**
  - `read-only` stops a Loop `bash` call;
  - `accept-edits` edits without asking and still asks for `bash`;
  - `full-access` shows the confirmation and banner;
  - Codex in `ask` raises a permission request for a file edit;
  - a force-push hook blocks both Loop and Codex.

## 5. Decisions and open questions
- Decided: web tools run freely in `read-only`.
- Decided: a `tool_call` hook that errors or times out blocks the call (`onError: "block"` default).
- Open: workflow `ShellCommand` tasks (from `/workflow`, `agent_worker.rs:243`) run `host_env.exec` directly, skipping the gate, `check_command_policy` and the 120 s bash timeout. Proposal: route them through the gate as `bash` calls in PR 1.
