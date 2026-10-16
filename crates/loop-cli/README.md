# loop-cli

Interactive coding agent CLI for **Loop** by Soket AI.

Built on [`loop-ai`](../loop-ai) and [`loop-agent`](../loop-agent), with a ratatui TUI.

## Install / run

```bash
curl -fsSL https://loop.soket.ai/install | bash
loop
```

Or from source:

```bash
cargo run -p loop-cli
# or
cargo install --path crates/loop-cli
loop
```

On first start, Loop asks you to **connect a model provider** (unless a provider key is already set in the environment). Use `/login` any time to connect more:

| Provider | `/login` | Env var |
|---|---|---|
| Soket | `/login soket` | `SOKET_API_KEY` |
| OpenRouter | `/login openrouter` | `OPENROUTER_API_KEY` |
| OpenAI | `/login openai` | `OPENAI_API_KEY` |
| Any OpenAI-compatible API (Together, Groq, vLLM, Ollama, LM Studio, …) | `/login custom` (name, base URL, optional key) | — |
| TinyFish (web search) | `/login tinyfish` | `TINYFISH_API_KEY` |
| Tavily (web search) | `/login tavily` | `TAVILY_API_KEY` |
| Brave Search (web search) | `/login brave` | `BRAVE_API_KEY` |

Models are listed from each provider's `/models` endpoint; `/model` shows Soket first, then other connected providers. `/logout <provider>` disconnects one. Keys are stored in `~/.loop/agent/auth.json` (mode `0600`); custom providers are saved in `settings.json`. The web-search keys enable the `web_search` tool (see [Web tools](#web-tools)).

Select a model with `/model`. Soket uses `https://api.soket.ai/v1`; its model catalog is refreshed from `GET /v1/models` and cached in `models-store.json`.

## Config layout

| Path | Purpose |
|------|---------|
| `~/.loop/agent/settings.json` | Defaults (model, theme, sandbox, skills…) |
| `~/.loop/agent/auth.json` | API keys |
| `~/.loop/agent/models.json` | Extra OpenAI-compat providers (hand-written; `models` optional — listed from `/models`) |
| `~/.loop/agent/models-store.json` | Dynamic catalog cache |
| `~/.loop/agent/themes/*.json` | Custom themes (JSON color tokens) |
| `~/.loop/agent/skills/` | Agent Skills (`SKILL.md`) |
| `~/.loop/agent/prompts/` | Prompt templates → `/name` |
| `~/.loop/agent/extensions/*.rhai` | Rhai extensions |
| `~/.loop/agent/hooks/*.json` | Declarative hooks |
| `~/.loop/agent/sessions.db` | SQLite sessions |
| `.loop/` (project) | Project overlays (requires trust) |

Override root with `LOOP_CODING_AGENT_DIR`.

Skills are discovered from `~/.loop/agent/skills`, `~/.agents/skills`, and `~/.claude/skills` (shared with Claude Code), plus `.loop/skills`, `.agents/skills`, and `.claude/skills` in trusted projects. Add extra directories under `"skills"` in settings:

```json
{ "skills": ["~/my-skills"] }
```

Turn individual skills on or off with `/skills` (an interactive picker: space or enter toggles), or `/skills enable <name>` / `/skills disable <name>`. `/skills list` shows every discovered skill and its state. Disabled skills are hidden from the model and from `/skill:` commands, and are saved under `disabledSkills` in the global `settings.json`:

```json
{ "disabledSkills": ["pdf", "frontend-design"] }
```

A project `.loop/settings.json` can add more names to `disabledSkills`; they are combined with the global list.

Context files are loaded automatically from the agent dir, `~/.claude/CLAUDE.md`, and the cwd and its ancestors. Each directory contributes all of `AGENTS.md`, `CLAUDE.md`, `CLAUDE.local.md`, and `.claude/CLAUDE.md` that exist. Symlinked or identical copies are included once.

## Slash commands (highlights)

`/theme`, `/sandbox`, `/model`, `/settings`, `/login`, `/logout`, `/websearch`, `/new`, `/review`, `/compact`, `/resume`, `/tree`, `/fork`, `/clone`, `/trust`, `/reload`, `/skills`, `/hotkeys`, `/help`, `/quit`, plus `/skill:name` and prompt templates.

## Local shell (`!command`)

Type `!ls -la` (or any shell command) to run it in the current sandbox/host environment. Output is shown in the transcript only — it is **not** sent to the model and is **not** added to session messages.

## Keybindings

| Shortcut | Action |
|----------|--------|
| `enter` | Send |
| `shift+enter` / `ctrl+j` | New line |
| `escape` | Interrupt |
| `ctrl+c` | Clear (twice quits) |
| `ctrl+l` | Model picker |
| `ctrl+p` | Cycle models |
| `shift+tab` | Cycle reasoning effort (also `/effort [level]`) |
| `ctrl+t` | Toggle thinking visibility |
| `ctrl+x` | Copy last assistant message |
| `ctrl+g` | External editor (`$EDITOR`) |
| `!command` | Run shell locally (not sent to the model) |

See `/hotkeys`. Override in `keybindings.json`.

## Themes

Ship `dark` and `light`. Custom JSON themes use the same color tokens. Change with `/theme name` or `settings.theme`.

## Sandbox

`/sandbox` / `/sandbox status` — boxed status (on/off, kind, runtime; not sent to the model)  
`/sandbox off` — host tools  
`/sandbox local` — same as `--full --runc` (rootless Podman + runc)  
`/sandbox local --partial` — host FS (jailed); only `bash` via `podman exec`  
`/sandbox local --full --runsc` — gVisor  
`/sandbox local --partial --krun` — libkrun microVM  

The project workdir is bind-mounted at the **same absolute path** inside the container (e.g. `/home/you/proj` → `/home/you/proj`), so `@file` tags and tool paths stay identical on host and guest.

**Runtimes** (pick one):
- `--runc` (default) — rootless containers; needs `podman` + `runc`
- `--crun` — same model with `crun`
- `--runsc` / `--gvisor` — gVisor; needs `runsc`
- `--krun` — microVM; needs `crun-krun` + `/dev/kvm`

**Isolation** (pick one): `--full` (default) or `--partial`.

If deps for the chosen runtime are missing, Loop prints install instructions and leaves sandbox **off**. Startup with `"mode": "local"` falls back to off with a warning. Remote sandbox is reserved (`/sandbox remote …`).

```json
{ "sandbox": { "mode": "local", "isolation": "full", "runtime": "runc" } }
```

## File edit review / tool approval

On **new sessions**, `write` / `edit` / `bash` / `web_fetch` / `web_search` pause for approval (per `toolPermissions`). File edits open Cursor or VS Code with `--diff`. Options:

1. **Accept** — this change only  
2. **Accept all … for this session** — remembered on the session (survives resume)  
3. **Reject** — revert / block; **Tab** adds a reason for the model

```json
{
  "fileEditReview": "newSession",
  "diffEditor": "cursor",
  "toolPermissions": {
    "write": "ask",
    "edit": "ask",
    "bash": "ask",
    "read": "allow"
  }
}
```

- `fileEditReview`: `newSession` (default) · `always` · `never` — when `ask` tools prompt  
- `toolPermissions`: per-tool `ask` | `allow` | `deny` in `~/.loop/agent/settings.json`  
- Interactive workflows are wired for `write`, `edit`, `bash`, `web_fetch`, and `web_search`
- `/review [newSession|always|never]` toggles the ask policy

## Web tools

`web_fetch` is built in: it fetches an http(s) URL and returns the body as text
(HTML pages become plain text). Only public internet hosts are reachable —
loopback, private, link-local and reserved addresses are refused before any
connection (redirects re-checked per hop), and bodies are capped and truncated.
It runs in the Loop process, not inside the sandbox.

`web_search` returns ranked results (title, URL, snippet). Connect a backend
with the login wizard — the key is verified with a probe search, stored in
`auth.json`, and the tool is enabled immediately (no restart):

| Provider | Connect | Env alternative |
|---|---|---|
| Soket (default) | `/login soket` | `SOKET_API_KEY` |
| Tavily | `/login tavily` | `TAVILY_API_KEY` |
| TinyFish | `/login tinyfish` | `TINYFISH_API_KEY` |
| Brave Search | `/login brave` | `BRAVE_API_KEY` |

`/logout <provider>` disconnects that provider until a successful `/login`,
even when its key is set in the environment. `/logout soket` disconnects
Soket models, search, and fetch; automatic fetching then uses direct requests.
A fetch explicitly pinned to Soket is unavailable until login. Other search
providers and subscription agents retain their own tools. External tool cards
show the subscription name, such as `ChatGPT · Web search`.

`/websearch` and `--no-web` control Loop's own tools, not a subscription agent's tools. Hand-written config in
`settings.json` is also supported:

```json
{
  "webFetch": { "allowPrivateAddresses": false },
  "webSearch": { "provider": "tavily" }
}
```

- `webFetch.allowPrivateAddresses`: `true` permits localhost/intranet URLs (for
  self-hosted mirrors; fails closed by default)
- `webSearch.apiKeyEnv`: custom env-var names, checked before the saved key
- `webSearch.baseUrl`: point at a self-hosted search proxy
- `webSearch.allowPrivateAddresses`: allow a private `baseUrl`
- Both tools default to `ask` in `toolPermissions`; the approval prompt shows
  the URL / query, with an *Accept all web requests for this session* option
- Key resolution order: `webSearch.apiKeyEnv` env vars → the saved `/login`
  key → the provider's default env var. With none present, Loop starts
  without `web_search` and prints a note

## Diagnostics

Interactive mode keeps diagnostic logs out of the terminal UI. Recoverable
reasoning fallbacks appear as a short notice once per model. Use `--debug`
to save detailed logs under `target/debug/logs`; print mode still logs to stderr.

## Extensions & hooks

- **Rhai**: `register_command`, `notify`, stubs for `register_tool` / `on` / …
- **JSON hooks**: `on` lifecycle names (`before_agent_start`, `session_before_compact`, …)

Examples under [`examples/`](examples/).

## CLI flags

```
loop [--provider soket] [--model qwen3-30b] [--theme dark]
     [--resume <id>] [--print "prompt"] [--no-context-files]
loop config
```

## Tests

```bash
cargo test -p loop-cli
cargo test -p loop-ai
cargo test -p loop-agent
```
