# loop-acp

Runs any [Agent Client Protocol](https://agentclientprotocol.com) (ACP v1)
agent as a Loop backend: Codex through `codex-acp`, Claude through
`claude-agent-acp`, Gemini CLI, Hermes, OpenCode and others. Nothing here
depends on which agent it talks to; an agent is an `AgentConfig` (command,
arguments, environment).

- `AcpBackend` implements `loop_protocol::AgentBackend`. It owns the
  connection → session → turn → pending-request state machine:
  - every prompt ends with exactly one `TurnEnded` and one `TurnOutcome`;
  - each process start is a new generation, and answers for an older
    generation are never sent;
  - if the agent process is lost mid-turn, every live turn in it ends as
    `Uncertain` and the prompt is never re-sent;
  - `cancel` is fire-and-forget. Pending permission requests are answered
    `cancelled`, and a turn that ignores the cancel past the agent's
    `cancel_timeout` gets its process tree killed.
- `ProcessConnector` starts the agent in its own process group (Unix) or
  kills its tree with `taskkill /T` (Windows). The agent gets a minimal
  environment: no provider API keys unless its config sets them.
- `mapping` translates ACP types to `loop-protocol` and synthesizes the
  message boundaries ACP does not have.
- `rpc` is line-delimited JSON-RPC 2.0. Non-JSON lines are skipped, oversized
  lines are dropped, and unknown requests get a `-32601` error so the agent
  never hangs waiting.

Loop offers agents file access and terminals (`fs/*`, `terminal/*`) only
when its `HostServices` provides them, and then they run through Loop's own
approval, review and sandbox. Otherwise the agent does its own execution.

Types come from the official `agent-client-protocol-schema` crate; the
transport and process supervision are Loop's own, on tokio.

Tests (`tests/acp_backend.rs`) run against a fake agent that speaks raw ACP
JSON-RPC, so no real agent binary is needed.
