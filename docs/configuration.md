# Configuration

Global configuration is in `~/.loop/agent/`. Run `loop config` to print the paths.

| Path | Content |
|---|---|
| `settings.json` | Settings (see below) |
| `auth.json` | API keys (file mode `0600`) |
| `models.json` | Extra OpenAI-compatible providers, written by hand |
| `sessions.db` | Saved sessions (SQLite) |
| `skills/`, `prompts/`, `hooks/`, `extensions/`, `themes/` | Skills (`SKILL.md`), prompt templates, hooks, extensions, themes |

In `models.json`, custom provider `models` can be string ids (listed with metadata from `/models`) or objects that pin details the listing omits. Object entries require `id`; `name`, `contextWindow`, `maxTokens`, and `reasoning` are optional. The context and token fields also accept snake_case names.

```json
{
  "providers": [{
    "id": "local",
    "baseUrl": "http://localhost:8000/v1",
    "models": [{ "id": "my-model", "name": "My model", "contextWindow": 131072, "maxTokens": 8192, "reasoning": true }]
  }]
}
```

A project can add `.loop/settings.json` in its root. Project values override global values. Arrays replace, they do not merge.

Common settings in `settings.json`:

| Key | Default | Purpose |
|---|---|---|
| `defaultProvider`, `defaultModel` | none | Provider and model at startup |
| `toolPermissions` | `write`, `edit`, `bash`: `ask`; `read`: `allow` | Per-tool permission: `allow`, `ask` or `deny` |
| `fileEditReview` | `newSession` | When `ask` tools prompt: `newSession`, `always` or `never` |
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
