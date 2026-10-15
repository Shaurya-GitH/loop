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

Models are listed from each provider's `/models` endpoint; `/model` shows Soket first, then other connected providers. `/logout <provider>` disconnects one. Keys are stored in `~/.loop/agent/auth.json` (mode `0600`); custom providers are saved in `settings.json`.

Default provider/model: **`soket` / `qwen3-30b`** at `https://api.soket.ai/v1`. The model catalog is refreshed from `GET /v1/models` and cached in `models-store.json`.

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

`/theme`, `/sandbox`, `/model`, `/settings`, `/login`, `/logout`, `/new`, `/review`, `/compact`, `/resume`, `/tree`, `/fork`, `/clone`, `/trust`, `/reload`, `/skills`, `/hotkeys`, `/help`, `/quit`, plus `/skill:name` and prompt templates.

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

On **new sessions**, `write` / `edit` / `bash` pause for approval (per `toolPermissions`). File edits open Cursor or VS Code with `--diff`. Options:

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
- Interactive workflows are wired for `write`, `edit`, and `bash` (others can be configured for later)  
- `/review [newSession|always|never]` toggles the ask policy

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
