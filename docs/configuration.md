# Configuration

Global configuration is in `~/.loop/agent/`. Run `loop config` to print the paths.

| Path | Content |
|---|---|
| `settings.json` | Settings (see below) |
| `auth.json` | API keys (file mode `0600`) |
| `models.json` | Extra OpenAI-compatible providers, written by hand |
| `sessions.db` | Saved sessions (SQLite) |
| `skills/`, `prompts/`, `hooks/`, `extensions/`, `themes/` | Skills (`SKILL.md`), prompt templates, [hooks](hooks.md), extensions, themes |

A project can add `.loop/settings.json` in its root. Project values override global values. Arrays replace, they do not merge.

Common settings in `settings.json`:

| Key | Default | Purpose |
|---|---|---|
| `defaultProvider`, `defaultModel` | none | Provider and model at startup |
| `permissionMode` | `ask` | `read-only`, `ask`, `accept-edits` or `full-access`; see [permissions](permissions.md). A project may only choose a stricter mode |
| `toolPermissions` | `write`, `edit`, `bash`: `ask`; `read`, `web_fetch`, `web_search`: `allow` | Per-tool rules, `allow`, `ask` or `deny`, with optional patterns such as `bash(git status*)`. `deny` applies in every mode; with nobody to ask, `ask` refuses the call. A project may only add `ask`/`deny` rules |
| `bash.defaultTimeoutMs`, `bash.maxTimeoutMs` | `120000`, `600000` | `bash` time limits; a call may ask for up to the maximum (global only) |
| `fileEditReview` | `newSession` | Kept for old settings files: with no `permissionMode`, `never` starts in `accept-edits`. Other values change nothing |
| `sandbox.mode` | `off` | `off` or `local` (Linux only) |
| `compaction` | enabled | `reserveTokens`, `keepRecentTokens` for `/compact` |
| `skills`, `disabledSkills` | empty | Extra skill paths, and skills to hide |
| `mcpServers`, `disabledMcpServers` | empty | MCP servers (stdio `command` or remote `url`) |
| `providers` | empty | Custom providers added with `/login custom` |

Environment variables:

| Variable | Purpose |
|---|---|
| `SOKET_API_KEY`, `OPENROUTER_API_KEY`, `OPENAI_API_KEY` | Provider API keys |
| `LOOP_CODING_AGENT_DIR` | Use a different config directory |
| `LOOP_CODING_AGENT_SESSION_DIR` | Use a different directory for `sessions.db` |
| `LOOP_DEBUG=1` | Write debug logs (same as `--debug`) |
