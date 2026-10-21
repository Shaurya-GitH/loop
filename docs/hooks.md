# Hooks

A hook is a JSON file in `~/.loop/agent/hooks/` or, in a trusted project, `.loop/hooks/`. It names an event, optional `match` rules, and an action. Problems with a hook file (invalid JSON, an unknown event, a risky rule) are shown once when Loop starts.

```json
{
  "name": "no-force-push",
  "on": "tool_call",
  "match": { "kind": "execute", "args": { "command": "*git push*--force*" } },
  "action": { "block": true, "reason": "Force pushes are not allowed here" }
}
```

## Events

| `on` | When | What the action can do |
|---|---|---|
| `tool_call` | Before a tool runs | `block` (the reason goes to the model) or `allow` (no prompt) |
| `tool_result` | After a tool ran | `appendContext`: text added to the result the model sees |
| `before_agent_start` | Before a turn | `block` cancels the turn |
| `session_before_compact`, `session_before_tree` | Before compaction or tree navigation | `block` cancels; `summary` supplies one |
| `settled`, `queue_update`, `shutdown_requested` | Lifecycle | — |

Tool events cover Loop's own tools, MCP tools, `mcp serve` clients and `/workflow` tasks, in every mode. For subscription agents (ChatGPT):

- **`tool_call`** fires when the agent asks permission. A block or allow picks the agent's own "reject once" or "allow once" option.
- **`tool_result`** fires for every agent tool call that finishes, as an observer only: the agent has already run it.

## `match`

Every rule you set must match. An unknown field is an error.

| Rule | Example | Matches |
|---|---|---|
| `tool` | `"bash"`, `"write\|edit"`, `"mcp__github__*"` | Tool name glob; `\|` separates alternatives |
| `kind` | `"execute"` | `read`, `edit`, `delete`, `move`, `search`, `execute`, `fetch`, `think`, `other` |
| `source` | `"native"`, `"mcp:github"`, `"agent:chatgpt"` | Where the tool comes from |
| `mode` | `"ask\|accept-edits"` | The [permission mode](permissions.md) |
| `args` | `{"command": "*--force*", "/path": "src/*"}` | An argument (by key, or a JSON pointer starting with `/`) matched as a glob |
| `argsContains` | `"--force"` | Deprecated: a substring of the JSON arguments |

A `tool_call` hook with `block` and no `match` blocks **every** tool call. Loop warns when it loads one.

## Precedence

- **Deny rules beat everything.** A `toolPermissions` deny wins over a hook's `allow`, and `read-only` mode can't be overridden by a hook.
- **A hook's `block` applies in every mode,** including `full-access`.
- **When several hooks match,** any block wins over an allow. Block reasons and added context are joined.
- **Hooks can't change a tool's arguments.**

## Command hooks

An action can run a program instead of, or as well as, a fixed decision:

```json
{
  "on": "tool_call",
  "match": { "kind": "edit" },
  "action": { "command": ["python3", "~/.loop/agent/hooks/check_edit.py"], "timeoutMs": 10000 }
}
```

- **`command`** is an argument list, never a shell string, with `~/` expanded. It runs in the session's directory with these variables set: `LOOP_HOOK_EVENT`, `LOOP_SESSION_ID`, `LOOP_PROJECT_DIR`, `LOOP_PERMISSION_MODE`.
- **stdin** gets the event as JSON:

  ```json
  {"event": "tool_call", "source": {"type": "native"}, "tool": "bash", "kind": "execute",
   "args": {"command": "cargo test"}, "callId": "…", "sessionId": "…", "cwd": "…", "mode": "ask"}
  ```

  `tool_result` events add `isError` and `output` (capped at 64 KB).
- **The exit code** decides:

  | Exit | Meaning |
  |---|---|
  | `0`, empty stdout | No decision |
  | `0`, JSON stdout | `{"decision": "allow" \| "block", "reason": "…", "appendContext": "…"}` |
  | `2` | Block, with stderr (first 4 KB) as the reason |
  | anything else | An error |
- **Errors:** stdout over 64 KB or that isn't JSON, a crash, or a timeout count as errors. `onError` decides what an error means: `block` (the default for `tool_call`) or `ignore` (the default for every other event). So a broken or slow `tool_call` hook blocks the call rather than letting it through.
- **`timeoutMs`** defaults to 10 seconds, with a maximum of 2 minutes. The hook runs in its own process group (Unix). On timeout, or when you cancel the turn, Loop kills it and everything it started.
- **Hooks run before each call, one call at a time,** so a slow hook delays the calls after it. Keep `tool_call` hooks quick.

## Trust

Project hooks load only in projects you trust. Since hooks can run commands, trusting a project lets it run programs through its hooks.
