# loop-protocol

The event model every Loop agent backend produces and every surface reads.

Loop's own agent (`NativeBackend`) and external agents reached over the Agent
Client Protocol (`AcpBackend`) both report a turn as a stream of
`UpdateEnvelope`s. The TUI, `--print` and later the desktop app render only
these events, so adding a backend never adds surface code.

- `SessionUpdate`: message and thought chunks with explicit message
  boundaries, tool calls and their updates, plans, usage, notices,
  interactions and the end of a turn.
- `UpdateEnvelope`: the update plus its backend instance, process generation,
  session, turn, sequence number and whether it is live or replayed history.
- `InteractionRequest` / `InteractionOutcome`: permission prompts, questions,
  forms and URL flows. Native option ids are kept verbatim.
- `ErrorInfo`: a category and a retryable flag for failed turns.
- `Capabilities`: what a backend supports.

The vocabulary follows ACP (tool kinds, diff content, permission option kinds)
so ACP maps across almost one to one.

## Wire format

JSON with snake_case field names. Tagged enums use a `type` field
(`kind` for interactions, `outcome` for interaction outcomes). The format is
versioned by `PROTOCOL_VERSION`, carried in every envelope as `v`.

Readers are lenient: an update, content block or tool content with an unknown
`type` deserializes to its `Unknown` variant with the raw JSON kept, and
unknown enum values such as a new tool kind become `Other`. A newer producer
therefore never breaks an older reader.

## Backend interface

`backend::AgentBackend` is what every backend implements: sessions, `prompt`
(returning a `TurnHandle` that resolves exactly once), fire-and-forget
`cancel`, and one update stream for all sessions. Operations not every
backend has (`Compaction`, `SessionStats`, `LocalShell`) are separate traits
reached through accessors that return `None` by default.

This crate depends on `serde`, `serde_json`, `async-trait` and `tokio`'s
sync primitives. It does not depend on any runtime, agent or surface crate.

See [External agents](../../../docs/design/external-agents.md) for the design.
