# loop

[![CI](https://github.com/soketlabs/loop/actions/workflows/ci.yml/badge.svg?branch=main)](https://github.com/soketlabs/loop/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/soketlabs/loop?sort=semver)](https://github.com/soketlabs/loop/releases/latest)
[![License](https://img.shields.io/github/license/soketlabs/loop)](LICENSE)
[![Docs](https://img.shields.io/badge/docs-loop.soket.ai-blue)](https://loop.soket.ai/)

Open-source Rust agent harness by [**Soket AI**](https://soket.ai): an agent loop with tool use, sessions, sandboxing, and a unified LLM provider API. Designed to be performant and memory-efficient for long-running agents on the most critical workloads.

<picture>
  <img src="resources/loop.gif" alt="Loop demo" width="420">
</picture>

Docs: [https://loop.soket.ai/](https://loop.soket.ai/)

## Status

loop is a **developer preview (alpha)**. Settings, session files and CLI flags can change without notice before 1.0. Report problems in [GitHub Issues](https://github.com/soketlabs/loop/issues).

## Crates

| Crate | Path | Role |
|-------|------|------|
| **loop-ai** | [`crates/loop-ai`](crates/loop-ai) | Unified LLM API, Soket provider (`/v1/models` refresh), OpenAI-compat + faux |
| **loop-agent** | [`crates/loop-agent`](crates/loop-agent) | Agent loop, AgentHarness, tools, sessions, sandbox, skills |
| **loop-cli** | [`crates/loop-cli`](crates/loop-cli) | Interactive `loop` TUI (ratatui) |
| **loop-update** | [`crates/ops/loop-update`](crates/ops/loop-update) | Release checks, verified updates, rollback and uninstall |
| **loop-protocol** | [`crates/core/loop-protocol`](crates/core/loop-protocol) | Versioned event model and backend interface shared by every agent and surface |
| **loop-acp** | [`crates/extend/loop-acp`](crates/extend/loop-acp) | Run any Agent Client Protocol agent (Codex, Claude, Gemini, …) as a backend |

## Quick start

Install the CLI from GitHub Releases:

```bash
curl -fsSL https://loop.soket.ai/install | bash
```

That downloads the latest `loop` binary for your OS into `~/.loop/bin` and adds it to `PATH`. Pin a version or skip PATH edits:

```bash
curl -fsSL https://loop.soket.ai/install | bash -s -- --version 0.3.4
curl -fsSL https://loop.soket.ai/install | bash -s -- --no-modify-path
```

From a clone of this repo you can also run `./install.sh`. Assets come from [soketlabs/loop releases](https://github.com/soketlabs/loop/releases).

On native Windows, run the PowerShell installer from a clone:

```powershell
.\install.ps1
.\install.ps1 -Version 0.3.4 -NoModifyPath
```

For installer-owned binaries, opening the terminal chat checks GitHub in the background. If a newer
release is available, it shows a notice with the `loop update` command. It does
not install updates at startup. Offline checks do not block the chat.

```bash
loop update --check          # show the latest release
loop update                  # verify and install it
loop rollback                # restore the previous binary
loop uninstall               # remove program files; keep all user data
```

See [Install and update](docs/install-update.md) for custom paths, pinned
versions, recovery and package-manager installs.

Build from source:

```bash
cargo run -p loop-cli
```

First run asks you to connect a model provider: Soket, OpenRouter, OpenAI, or any OpenAI-compatible API (`/login` later to add more). Alternatively set `SOKET_API_KEY`, `OPENROUTER_API_KEY` or `OPENAI_API_KEY`. Config lives under `~/.loop/agent/`. See [`crates/loop-cli/README.md`](crates/loop-cli/README.md).

### Supported platforms

| OS | Architecture | Status | Install |
|----|--------------|--------|---------|
| macOS (Apple Silicon) | arm64 | Supported | `curl -fsSL https://loop.soket.ai/install \| bash` |
| macOS (Intel) | x86_64 | Supported | `curl -fsSL https://loop.soket.ai/install \| bash` |
| Linux (glibc) | x86_64, arm64 | Supported | `curl -fsSL https://loop.soket.ai/install \| bash` |
| Linux via WSL | x86_64, arm64 | Supported (uses the Linux build) | `curl -fsSL https://loop.soket.ai/install \| bash` inside WSL |
| Windows (native) | x86_64 | Installer and lifecycle tests in CI | `./install.ps1` in PowerShell |
| Linux (musl, e.g. Alpine) | any | Not supported | [Build from source](#build--test) |

### Feature support

| Feature | Linux | macOS | Windows |
|---------|-------|-------|---------|
| Sandboxing | Supported | Planned | Planned |
| Compaction (manual) | Supported | Supported | In testing |
| mcp-serve | Supported | Supported | In testing |

The installer detects your OS and CPU, downloads the latest matching release from GitHub, verifies its SHA-256 checksum, installs to `~/.loop/bin`, and adds that directory to your `PATH`. Re-running it is safe: it skips the download if you already have the latest version (use `--force` to reinstall).

## Supported models

loop connects to model providers through an OpenAI-compatible Chat Completions API. Connect a provider on first run or with `/login`.

| Provider | Connect | API key variable |
|---|---|---|
| Soket | `/login soket` | `SOKET_API_KEY` |
| OpenRouter | `/login openrouter` | `OPENROUTER_API_KEY` |
| OpenAI | `/login openai` | `OPENAI_API_KEY` |
| Any OpenAI-compatible API (for example Together, Groq, vLLM, Ollama, LM Studio) | `/login custom` (name, base URL, optional key) | none |

The model list comes from each provider's `/models` endpoint. Use `/model` to select a model.

Not supported yet: the native Anthropic Messages API and the OpenAI Responses API.

## Configuration

Global configuration is in `~/.loop/agent/`. A project can override it with `.loop/settings.json`. Run `loop config` to print the paths.

See [docs/configuration.md](docs/configuration.md) for all files, settings and environment variables.

## Build / test

```bash
cargo build
cargo test -p loop-ai
cargo test -p loop-agent
cargo test -p loop-cli
```

### Tracing (optional `telemetry` feature)

OpenTelemetry tracing to Langfuse or any OTLP collector is off by default, so the standard build carries no OpenTelemetry dependencies. Build with the feature to get `/tracing` in the TUI and the `--trace-*` flags for benchmark runs with `--print`:

```bash
cargo build -p loop-cli --release --features telemetry
cargo test -p loop-telemetry -p loop-agent -p loop-app-core -p loop-cli \
  --features loop-agent/telemetry,loop-app-core/telemetry,loop-cli/telemetry
```

Maintainers: see [docs/releasing.md](docs/releasing.md) to cut a release.

## Contributing

Contributions are welcome. Read [CONTRIBUTING.md](.github/CONTRIBUTING.md) before you open a pull request.

## Community

- Questions and ideas: [GitHub Discussions](https://github.com/soketlabs/loop/discussions)
- Bugs and feature requests: [GitHub Issues](https://github.com/soketlabs/loop/issues)
- Documentation: [loop.soket.ai](https://loop.soket.ai/)
- Security issues: report privately, see [SECURITY.md](.github/SECURITY.md)

## License

Copyright 2026 Soket AI.

Licensed under the Apache License, Version 2.0. See [LICENSE](LICENSE).
