# Contributing to Loop

Thanks for helping build Loop. Loop is an agent runtime: it runs model-chosen shell commands, edits files, and spawns MCP servers on real machines. Because of that, we hold contributions to systems-software standards. This guide covers the rules and the reasons behind them.

- **Questions and design discussion:** [GitHub Discussions](https://github.com/soketlabs/loop/discussions)
- **Bugs and feature proposals:** [open an issue](https://github.com/soketlabs/loop/issues/new/choose) using one of the forms
- **Security vulnerabilities:** never in a public issue. See [Reporting security issues](#reporting-security-issues).
- **User docs:** <https://loop.soket.ai/>

All participants are expected to follow the [Contributor Covenant v2.1](https://www.contributor-covenant.org/version/2/1/code_of_conduct/).

## Table of contents

1. [Licensing and sign-off (DCO)](#licensing-and-sign-off-dco)
2. [Repository layout](#repository-layout)
3. [Prerequisites](#prerequisites)
4. [Local workflow](#local-workflow)
5. [Code standards](#code-standards)
6. [Security and sandboxing invariants](#security-and-sandboxing-invariants)
7. [Commit messages](#commit-messages)
8. [Pull requests](#pull-requests)
9. [Reporting security issues](#reporting-security-issues)

---

## Licensing and sign-off (DCO)

Loop is licensed under the [Apache License, Version 2.0](../LICENSE). Under Section 5 of that license, anything you intentionally submit is licensed under the same terms ("inbound = outbound"). **You keep the copyright to your contribution.** No copyright assignment is required.

We use the [Developer Certificate of Origin 1.1](https://developercertificate.org/) instead of a Contributor License Agreement (CLA):

- It is a per-commit statement that you have the right to submit the code. There are no forms to sign and no legal review for your employer to run.
- Combined with Apache-2.0's explicit patent grant, it gives downstream users of an agent runtime the provenance and patent protection they need.

**Every commit must carry a `Signed-off-by` trailer** whose name and email match the commit author:

```bash
git commit -s -m "fix(agent): propagate spawn errors from host shell"
# appends: Signed-off-by: Jane Doe <jane@example.com>
```

To fix commits you forgot to sign off:

```bash
git commit --amend -s --no-edit             # last commit
git rebase --signoff origin/main            # every commit on your branch
git push --force-with-lease
```

By signing off you certify the DCO, including that:

- You have the right to submit the code under Apache-2.0.
- It does not contain code copied from sources with incompatible licenses (for example, GPL-only code).
- It does not contain code you are not allowed to share.

AI-assisted contributions are welcome. **You** are the author of record: you must understand, test, and stand behind every line you sign off.

## Repository layout

Loop is a Cargo workspace. Put changes in the narrowest crate that fits.

| Crate | Responsibility |
|---|---|
| `loop-protocol` | Versioned event model and `AgentBackend` interface every backend and surface shares |
| `loop-ai` | Unified LLM provider API (Soket, OpenAI-compatible, OpenRouter, faux test provider) |
| `loop-agent` | Agent loop, `AgentHarness`, tools, sessions, skills, execution environments, sandbox backends |
| `loop-orchestration` | Multi-agent orchestration |
| `loop-mcp` | Model Context Protocol client/server integration |
| `loop-acp` | Runs any Agent Client Protocol agent (Codex, Claude, Gemini, …) as a backend |
| `loop-app-core` | Shared application runtime: config, tool approval, settings |
| `loop-cli` | The `loop` binary and its ratatui TUI |
| `loop-desktop` | GPUI desktop app (experimental, nightly toolchain) |
| `loop-telemetry` | Optional OpenTelemetry tracing (`telemetry` feature) |
| `loop-test-support` | Shared test fixtures and helpers |

Dependencies flow one way:

- `loop-protocol` is the base for backends and surfaces; it depends on no other Loop crate.
- `loop-acp` depends only on `loop-protocol`.
- `loop-ai` is the base of Loop's own agent.
- `loop-mcp`, `loop-orchestration`, and `loop-telemetry` depend only on `loop-ai`.
- `loop-agent` composes them, through optional features.
- `loop-app-core` builds on `loop-agent`.
- The front ends (`loop-cli`, `loop-desktop`) sit on top.

Lower crates must not depend on higher ones, and a library crate must never depend on a UI crate.

## Prerequisites

| Tool | Requirement |
|---|---|
| Rust | Latest **stable** via [rustup](https://rustup.rs/), with `rustfmt` and `clippy` components |
| MSRV | **1.88** (`rust-version` in the root `Cargo.toml`). Do not use language or `std` features newer than this without raising the MSRV in a dedicated PR. |
| Git | 2.30+ (for `rebase --signoff`) |
| Podman | Optional. Linux only, needed to work on or test the podman/krun sandbox backends |

```bash
rustup toolchain install stable --component rustfmt,clippy
```

There is no root `rust-toolchain.toml`: the workspace builds on stable, as CI does. `crates/loop-desktop` carries its own `rust-toolchain.toml` that pins **nightly**. When you work inside that directory, rustup switches toolchains automatically.

## Local workflow

```bash
git clone https://github.com/<you>/loop && cd loop
git remote add upstream https://github.com/soketlabs/loop
git switch -c fix/short-description

cargo run -p loop-cli                       # run the TUI from source
LOOP_DEBUG=1 cargo run -p loop-cli          # verbose logs -> ./target/debug/logs/
```

Before pushing, all of the following must pass:

```bash
# 1. Formatting
cargo fmt --all -- --check

# 2. Lints: warnings are errors
cargo clippy --workspace --exclude loop-desktop --all-targets --locked -- -D warnings

# 3. Tests (mirrors .github/workflows/ci.yml)
cargo test --locked                        # every crate except loop-desktop

# 4. Telemetry feature build + tests (if you touched tracing, spans, or loop-telemetry)
cargo test -p loop-telemetry -p loop-agent -p loop-app-core -p loop-cli \
  --features loop-agent/telemetry,loop-app-core/telemetry,loop-cli/telemetry --locked
```

Notes:

- CI (`.github/workflows/ci.yml`) runs the format and lint checks on Linux, and the tests on Linux, macOS and Windows. Run them locally first to avoid a red build.
- **`--locked` is mandatory.** Do not change `Cargo.lock` unless your PR intentionally adds or updates a dependency. For version bumps use `cargo update -w`, not `cargo generate-lockfile`; the GPUI git dependencies must not be re-resolved (see the comment in `Cargo.toml`).
- **Live tests** that call real providers or krun microVMs are opt-in through `LOOP_TEST_*` environment variables (for example `LOOP_TEST_BASE_URL`, `LOOP_TEST_MODEL`, `LOOP_TEST_KRUN`). They must skip cleanly when those variables are unset, and they must never run in default CI.
- If you touch `loop-desktop`, also run `cargo clippy -p loop-desktop -- -D warnings` from `crates/loop-desktop` (nightly).

## Code standards

### Errors

- **Library crates** (`loop-ai`, `loop-agent`, `loop-mcp`, `loop-orchestration`, `loop-app-core`, `loop-telemetry`) expose typed errors built with `thiserror`. `anyhow` is for binaries (`loop-cli`, `loop-desktop`) and tests only.
- Propagate errors with `?` and `map_err` into the crate's error type. Keep the underlying cause in the message, for example `ExecutionError::new(ExecutionErrorCode::SpawnFailed, format!("podman exec: {e}"))`.
- **No new `.unwrap()` in non-test code.** The agent loop is long-lived: a panic loses the user's session, and in the TUI it corrupts the terminal. Instead:
  - Use `?`, `ok_or_else`, `unwrap_or_else`, or a `match`.
  - If a value is genuinely infallible, use `.expect("invariant: <why this cannot fail>")` so the reason is reviewable.
  - Do not "fix" existing unwraps in unrelated code inside a feature PR. Send a separate `refactor:` PR.
- `.unwrap()` / `.expect()` are fine in tests, examples, and `const`/`static` initializers that are checked at compile time.

### `unsafe`

Avoid it. Any `unsafe` block needs a `// SAFETY:` comment explaining exactly which invariant makes it sound, and the PR description must justify why a safe alternative is not viable.

### Async and performance

Loop is designed for long-running agents, so memory growth and blocked executors are bugs.

- **Never block the Tokio runtime.** No `std::thread::sleep`, blocking file or network I/O, or `std::process::Command::output()` in async paths. Use `tokio::process`, `tokio::fs`, or `spawn_blocking`.
- Do not hold a `parking_lot` / `std` mutex guard across `.await`. Use `tokio::sync::Mutex` or restructure the code.
- Every long-running task must be cancellable (`CancellationToken`) and must not outlive its owner.
- Stream model and tool output; never buffer an unbounded response into memory. Throttle UI callbacks; see `OUTPUT_EMIT_INTERVAL` in `harness/env/host.rs`.
- Changes on hot paths (agent loop, streaming, session persistence, compaction) need before/after numbers in the PR: latency, allocations, or RSS over a long session.

### Style

- `rustfmt` defaults. Clippy must be clean with `-D warnings` for the code you touch. Only use `#[allow(clippy::…)]` with a comment explaining why.
- Public items carry `///` docs. Comments explain *why*, not *what*.
- Prefer `tracing` spans and fields to `println!`/`eprintln!`. In interactive TUI mode, stderr corrupts the display, so logging goes to file only.

### Tests

- Bug fixes include a regression test that fails without the fix.
- New tools, providers, and sandbox backends ship with tests that use the faux provider or mock clients (see `PodmanClient` and `loop-test-support`). They must not depend on the network or on a container runtime.
- Tests must be hermetic. Use `tempfile` for filesystem state, and never read or write the developer's real `~/.loop/`.

## Security and sandboxing invariants

Loop executes untrusted, model-generated actions. The following invariants are **mandatory**. A PR that weakens any of them needs explicit sign-off from a maintainer, and the reason must be recorded in the PR description.

### Execution environments and isolation

1. **All agent tool execution goes through an `ExecutionEnv`** (host or sandbox). Tools must not spawn processes, open files, or make network calls by side channels that bypass the active environment. Otherwise the sandbox is decorative.
2. **Sandbox backends** (the podman-based local sandbox with `runc` / `crun` / `runsc` / `krun` runtimes under `harness/sandbox/`) implement the `Sandbox` / `SandboxFactory` traits and must:
   - fail **closed**: if a sandbox was requested and cannot start, return a `SandboxError`; never silently fall back to host execution;
   - mount only the workspace that is explicitly configured, never `$HOME`, credential directories, or the container runtime socket;
   - report their real state through `SandboxStatus` / `SandboxInfo` so `/sandbox status` stays truthful.
3. Sandboxing is currently **Linux-only**. Code that is platform-specific must be `cfg`-gated, and it must report an unsupported mode clearly rather than pretend isolation exists.

### Subprocess handling

4. Every child process spawned on behalf of the agent uses `tokio::process::Command` with **`.kill_on_drop(true)`**, so cancelling a turn or dropping a session cannot leak processes.
5. Respect `inherit_env`. When it is false, call **`env_clear()`** and pass only the explicitly provided variables. Provider API keys (`*_API_KEY`) must never leak into tool subprocesses or sandboxes by default.
6. Pass arguments as an argv vector (`.arg()`/`.args()`). Do not build shell strings by interpolating model or user input. The single exception is the shell tool itself, which deliberately runs `sh -c` / `cmd /C` **after** tool approval.
7. Capture stdout and stderr through pipes, cap the output that goes back to the model, and enforce timeouts. Do not let a chatty or hung process stall the agent loop.

### Approval, secrets, and network exposure

8. **Tool approval is a security boundary.** File-edit and bash tools go through the approval flow in `loop-app-core/src/tool_approval.rs`. New tools with side effects must join an approval group. They must not default to auto-approved.
9. **Secrets never appear in** logs, traces, telemetry spans, error messages, session files, or panic messages. Redact them at the boundary, and test that redaction.
10. **Network listeners bind to `127.0.0.1` by default.** `mcp-serve` without an auth token must bind only to localhost. Exposing a listener on other interfaces requires authentication.
11. Treat MCP server output, model output, fetched web content, and skill files as **untrusted input**. They can contain prompt injection. Never let that content alter approval policy, sandbox configuration, or credentials.

### Safe error propagation

12. Errors that cross a tool boundary return to the model as structured tool errors (`ExecutionError` with an `ExecutionErrorCode`). They must not panic or abort the loop. A failing tool is a normal event the agent must be able to recover from.
13. Error messages that reach the model or a log must not include secrets, full environment dumps, or file contents from outside the workspace.

## Commit messages

We follow [Conventional Commits 1.0](https://www.conventionalcommits.org/):

```
<type>(<scope>): <imperative summary, ≤ 72 chars, no trailing period>

<body: what changed and why; wrap at 72 columns>

Fixes #123
Signed-off-by: Jane Doe <jane@example.com>
```

**Types:** `feat`, `fix`, `perf`, `refactor`, `test`, `docs`, `build`, `ci`, `chore`, `revert`.

**Scopes** (use the crate or subsystem): `ai`, `agent`, `sandbox`, `mcp`, `orchestration`, `app-core`, `cli`, `desktop`, `telemetry`, `install`, `release`.

**Breaking changes** to public APIs, config files, the session format, or CLI flags: add `!` after the type/scope **and** a `BREAKING CHANGE:` footer that describes the migration.

```
feat(agent)!: make Sandbox::start return SandboxInfo

BREAKING CHANGE: custom SandboxFactory impls must return SandboxInfo
from start(); wrap the previous return value in SandboxInfo::enabled().
```

## Pull requests

**One logical change per PR.** Small, atomic PRs get reviewed and merged quickly; large mixed ones stall.

- **Discuss first** for new tools, providers, sandbox backends, public trait changes, or anything that touches the security invariants. Open a feature request so the design is agreed before you write the code.
- **Do not mix concerns.** Keep reformatting, lint cleanup, dependency bumps, and renames out of feature or fix PRs, and send them separately. In particular, run `cargo fmt` / clippy fixes only on files your change touches.
- **Each commit builds and passes tests** on its own, is signed off, and follows Conventional Commits. Squash fixup commits before review is requested.
- **Rebase on `upstream/main`.** Do not merge `main` into your branch.
- **Describe the PR:** what changed, why, how you tested it (including OS and sandbox mode, if relevant), and any performance or security impact. Link the issue (`Fixes #123`).
- **Update docs** (crate `README.md`, `--help` text, and the docs site if user-facing) in the same PR as the behavior change.
- A maintainer approval is required to merge. Changes to `harness/sandbox/`, `harness/env/`, `tool_approval.rs`, credential handling, or `install.sh` need review from a maintainer familiar with that area.

### PR checklist

- [ ] Commits are signed off (`git commit -s`) and follow Conventional Commits
- [ ] `cargo fmt --all -- --check` passes
- [ ] `cargo clippy --workspace --exclude loop-desktop --all-targets --locked -- -D warnings` passes
- [ ] Tests pass with `--locked`; new behavior and fixes have tests
- [ ] No new `.unwrap()` in non-test code; any `unsafe` has a `// SAFETY:` comment
- [ ] Security invariants upheld (or deviations justified and flagged for maintainer review)
- [ ] Docs updated for user-visible changes

## Reporting security issues

**Do not open public issues for vulnerabilities.** That includes sandbox escapes, approval bypasses, credential leaks, prompt-injection paths that lead to unapproved execution, and anything in `install.sh` or the release pipeline.

Follow the process in [SECURITY.md](SECURITY.md).
