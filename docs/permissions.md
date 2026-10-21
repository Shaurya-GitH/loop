# Permissions

Every tool call Loop makes passes one check, whether it comes from the chat, `--print`, `mcp serve`, a `/workflow` task, or a subscription agent's permission request. The check runs in this order:

1. **Deny rules** (`toolPermissions` entries set to `deny`) and the `read-only` mode refuse the call.
2. **Hooks** (`tool_call`) may block the call or allow it without asking. See [hooks](hooks.md).
3. **The permission mode** allows the call or asks.
4. **Asking**: the TUI and desktop app show a prompt. With nobody to ask (`--print`, `mcp serve`), the call is refused, and the model is told which flag or rule would allow it.

## Modes

| Mode | read, search, web | write / edit | bash | MCP tools |
|---|---|---|---|---|
| `read-only` | allow | deny | read-only commands only | allow if the server marks the tool read-only, else deny |
| `ask` (default) | per `toolPermissions` | ask | ask | per `toolPermissions` |
| `accept-edits` | allow | allow inside the workspace; ask outside it and for `.git/`, `.loop/`, `.env*` | per `toolPermissions` | per `toolPermissions` |
| `full-access` | allow | allow | allow | allow |

- **The workspace** is the git root of the directory Loop runs in, or that directory itself. Paths are resolved through symlinks, so a link that points outside the workspace counts as outside.
- **`read-only` is a ceiling.** A hook can't allow a call that changes files or runs commands.
- **Bash in `read-only`** runs a command only when every part of it reads: `ls`, `pwd`, `cat`, `head`, `tail`, `wc`, `grep`, `rg`, `find`, `tree`, `file`, `stat`, `du`, `which`, `echo`, `sort`, `uniq`, `diff`, and `git status`, `log`, `diff`, `show`, `blame`, `ls-files`, `rev-parse`, and `branch` when it only lists. Output redirects (`>`), `$(…)`, backticks, variable assignments, and options that write or run programs (`find -exec`/`-delete`, `sort -o`, `rg --pre`, `git --output`) are refused.
- **MCP tools in `read-only`** run when their server marks them read-only (`readOnlyHint`). The server sets that hint, so deny a tool in `toolPermissions` if you don't trust it.
- **Session approvals:** "Accept all … for this session" in a prompt stops further prompts for that group (file edits, bash or web) in this session, including after `--resume`.
- **Deny rules and hooks apply in every mode,** `full-access` included.
- **Calls the mode allows** (for example, edits in `accept-edits`) also skip the after-edit review.

### Choosing a mode

- **Default for every session:** `"permissionMode": "accept-edits"` in `~/.loop/agent/settings.json`. A project's `.loop/settings.json` can only choose a stricter mode.
- **One run:** `loop --permission-mode read-only`.
- **This session:** `/permissions` lists the modes; `/permissions accept-edits` switches. The footer shows the current mode.

### `full-access`

- **Switching:** `/permissions full-access` asks you to repeat the command before it switches.
- **While on:** the footer shows a red `⚠ full access · tools run without approval` banner.
- **Startup:** starting in this mode prints a warning.
- **Unattended runs** (`--print`, `mcp serve`) need the flag `--permission-mode full-access`. The setting alone falls back to `ask` there, with a notice.
- **`mcp serve` without `--token`:** it warns, because any local process could then run tools without approval.

## Rules

`toolPermissions` maps a tool to `allow`, `ask` or `deny`. A key can carry a pattern on the call's main argument: the command for `bash`, the path for `read`/`write`/`edit`, the URL for `web_fetch`, the query for `web_search`.

```json
{
  "toolPermissions": {
    "bash": "ask",
    "bash(git status*)": "allow",
    "bash(cargo test*)": "allow",
    "bash(rm -rf *)": "deny",
    "write(~/secrets/*)": "deny",
    "write(src/generated/*)": "ask",
    "web_fetch(https://docs.rs/*)": "allow",
    "mcp__slack__*": "deny"
  }
}
```

- **Which rule wins:** a pattern rule beats the plain tool rule. Among matching pattern rules, `deny` beats `ask`, which beats `allow`.
- **Globs:** `*` matches any characters, including `/`; `?` matches one character.
- **Bash commands** are split at `;`, `&&`, `||`, `|`, `&` and newlines that are outside quotes:
  - A `deny` or `ask` pattern applies if **any** part matches it.
  - An `allow` pattern applies only if **every** part matches an `allow` pattern. So `git status && rm -rf x` still asks.
  - A command that uses `$(…)`, backticks or `<(…)` never matches an `allow` pattern.
- **A pattern `ask` still asks in `accept-edits`.**
- **Loop has an approval prompt only for `write`, `edit`, `bash`, `web_fetch` and `web_search`.** An `ask` rule on any other tool (for example `read(~/.ssh/*)` or an MCP tool) refuses the call. Use `allow` or `deny` for those.
- **A project's `.loop/settings.json`** can add `ask` and `deny` rules or make a rule stricter. It can't add `allow` rules or loosen a global rule.

## Subscription agents

Agents such as ChatGPT (codex-acp) run their own tools. Loop sets the agent's own mode to match:

| Loop mode | Codex mode |
|---|---|
| `read-only`, `ask` | `read-only` (Codex asks before edits and network use) |
| `accept-edits` | `workspace-write` |
| `full-access` | `agent-full-access` |

The agent's permission requests then pass through hooks and the mode:

- **A hook block or allow** chooses the agent's own "reject once" or "allow once" option, never an "always" one.
- **`full-access`** allows once.
- **`read-only`** rejects edits and commands.
- **Otherwise**, you are asked.

Codex runs some commands without asking: read-only commands in `read-only` mode, and sandboxed commands in `workspace-write`. Loop sees those only as tool-call updates, after they run.

## Upgrading from 0.4

- **`--print` and `mcp serve` refuse tools that need approval.** In `ask` mode that is `write`, `edit` and `bash`. The model is told why. For unattended edits use `--permission-mode accept-edits`. Bash still needs `full-access` or an allow rule such as `"bash(cargo test*)": "allow"`.
- **Resumed sessions ask too.** Before, `fileEditReview: newSession` (the default) turned prompts off after `--resume`. Now the mode decides; approvals you accepted for the session still apply.
- **`fileEditReview: never`** turned off every prompt. With no `permissionMode` set, it now starts Loop in `accept-edits`, with a notice at each start; setting `permissionMode` removes the notice. `newSession` and `always` no longer change anything, and `/review` only shows the session's approvals.
- **`web_fetch` and `web_search` are allowed by default.** A settings file that already says `ask` for them keeps asking. To be asked before each request, set `"web_fetch": "ask"` and `"web_search": "ask"` in `toolPermissions`.
- **A hook file with a `tool_call` block and no `match`** now blocks every tool call. Loop warns about such files at startup.
- **ChatGPT (Codex) in `ask` mode** runs in Codex's `read-only` mode, so it asks before each edit. Before, it ran in Codex's own default, which edits without asking.
- **A project's `.loop/settings.json`** can no longer loosen `toolPermissions` or `fileEditReview`.

## `bash` time limits

A `bash` call may pass `timeoutMs` to get more time than the default. A command that times out tells the model how to ask for more.

| Setting | Default | Purpose |
|---|---|---|
| `bash.defaultTimeoutMs` | `120000` | Timeout when a call names none |
| `bash.maxTimeoutMs` | `600000` | The most a call may ask for |

Both are global-only. `/workflow` shell tasks use the default.
