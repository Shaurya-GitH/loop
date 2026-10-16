# loop-agent

Stateful agent with tool execution and event streaming, built on [`loop-ai`](../loop-ai). Implemented in pure Rust for a unified distributable binary.

## Components

| Layer | Module | Role |
|-------|--------|------|
| Loop | `agent_loop` | Turns, parallel/sequential tools, steering/follow-up |
| Agent | `agent` | Stateful transcript, awaited subscribers, queues |
| Harness | `harness::AgentHarness` | Sessions, snapshots, sandbox, resources |
| Sessions | `harness::session` | Memory, JSONL, SQLite (+ FTS), scanning search |
| Tools | `harness::tools` | `read` / `write` / `edit` / `bash` over `ExecutionEnv` |
| Web tools | `harness::tools::web` | `web_fetch` / `web_search` with an SSRF-guarded HTTP client (`web` feature, on by default) |
| Sandbox | `harness::sandbox` | Pluggable env for tool side-effects (`KrunSandbox` / local krun) |

## Quick start

```rust
use std::sync::Arc;
use loop_agent::{
    stream_fn_from_models, Agent, AgentOptions, AgentState,
};
use loop_ai::{Models, providers::{faux_provider, FauxScript, FauxResponse}};

# async fn demo() {
let script = FauxScript::new();
script.push(FauxResponse::Text("hi".into()));
let models = Models::new();
models.set_provider(faux_provider(script));
let model = models.get_model("faux", "faux-model").unwrap();

let agent = Agent::new(AgentOptions::new(
    AgentState::new(model),
    stream_fn_from_models(Arc::new(models)),
));
agent.prompt("Hello").await.unwrap();
# }
```

## Web tools

`web_fetch` and `web_search` run in the harness process (host network), not
inside the sandbox environment. Every outbound URL passes the SSRF guard in
`harness::tools::web::guard`:

- `http`/`https` only, no URL credentials;
- literal-IP hosts must be public; hostnames are resolved through a validating
  resolver, so a connection is only ever established to a public address
  (redirects are followed manually and each hop is re-validated — DNS
  rebinding included);
- downloads are capped (2 MB) and truncated to the model's tool-output budget.

The default policy fails closed. `WebFetchConfig { allow_private_addresses: true }`
opts a deployment into intranet mirrors (and the same field on
`WebSearchConfig` allows private search-API base URLs).

`web_search` is backend-driven. The registry in `harness::tools::web::search`
lists `soket` (the default, using the Soket model-provider key), `tavily`,
`tinyfish` and `brave`.

`web_fetch` takes an optional `SoketWebApi`: with one (and
`WebFetchProvider::Auto` or `Soket`), pages are fetched by Soket's fetch API
and returned as markdown; the target URL still passes the strict guard before
it is sent. Without one, or with `WebFetchProvider::Direct` or
`allow_private_addresses`, the guarded direct client above is used.

## Sandbox

When harness sandbox mode is enabled, tool FS/shell ops use `sandbox.env()`:

```rust
use loop_agent::harness::{KrunIsolation, KrunSandbox, LocalSandboxRuntime, SandboxMode};
// SandboxMode::Enabled { sandbox: Arc::new(KrunSandbox::new(
//     KrunSandbox::config_for(workdir, KrunIsolation::Full, LocalSandboxRuntime::Runc))) }
```

`KrunSandbox` (`kind` = `local`) runs a Podman container with a selectable OCI runtime:

- **Runtimes:** `runc` (default), `crun`, `runsc` (gVisor), `krun` (microVM)
- **full** — `read` / `write` / `edit` / `bash` via `podman exec`
- **partial** — FS on the host bind-mount (path-jailed); only `bash` via `podman exec`

A future `remote` kind is reserved but not implemented.

## Sessions

- **Memory** — tests / ephemeral
- **JSONL** — one file per session, append-only tree entries (`create_jsonl_session_store`)
- **SQLite** — durable multi-session DB with migrations + optional FTS (`create_sqlite_session_store`)

## Live tests

```bash
export SOKET_API_KEY="your-key"
LOOP_TEST_BASE_URL="https://api.soket.ai/v1" \
LOOP_TEST_MODEL="qwen3-30b" \
LOOP_TEST_API_KEY_ENV="SOKET_API_KEY" \
cargo test -p loop-agent --test live_agent -- --ignored --nocapture
```

| Variable | Meaning |
|---|---|
| `LOOP_TEST_BASE_URL` | Base URL of any OpenAI-compatible API |
| `LOOP_TEST_MODEL` | Model id on that API |
| `LOOP_TEST_API_KEY_ENV` | The **name** of the environment variable that holds the key, not the key |

Live tests stream model text to stderr and run **3 turns** each (tool loop + harness/SQLite).

## Offline tests

```bash
cargo test -p loop-agent
```
