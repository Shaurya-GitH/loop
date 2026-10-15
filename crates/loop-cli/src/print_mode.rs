//! Headless `--print` runs, each recorded as one Langfuse trace when built with the
//! `telemetry` feature.

#[cfg(feature = "telemetry")]
use std::collections::BTreeMap;
use std::future::Future;

use loop_agent::AgentMessage;
use loop_ai::{AssistantContent, Message};
use loop_protocol::{
    AgentBackend, ContentBlock, Role, SessionRef, SessionUpdate, StopReason, TurnOutcome,
};
#[cfg(feature = "telemetry")]
use loop_telemetry::{obs, ObservationExt, TelemetryHandle, TraceAttrs};
use tokio::sync::broadcast::error::RecvError;
#[cfg(feature = "telemetry")]
use tracing::Instrument;

use crate::{CliRuntime, PendingAgent};

/// Name of the root observation of a print-mode trace.
#[cfg(feature = "telemetry")]
pub const ROOT_SPAN_NAME: &str = "loop.print";
/// Prefix of the stderr line carrying the trace id, for benchmark runners.
pub const TRACE_ID_PREFIX: &str = "loop-trace-id: ";

/// Per-run trace labels for `--print`; without the `telemetry` feature there are none.
#[cfg(not(feature = "telemetry"))]
#[derive(Debug, Clone, Default, PartialEq, Eq, clap::Args)]
pub struct TraceArgs {}

/// Per-run trace labels for `--print` (e.g. benchmark run and task ids).
#[cfg(feature = "telemetry")]
#[derive(Debug, Clone, Default, PartialEq, Eq, clap::Args)]
pub struct TraceArgs {
    /// Langfuse trace name (default: `loop.print`).
    #[arg(long = "trace-name", global = true)]
    pub name: Option<String>,
    /// Langfuse trace tag; repeatable.
    #[arg(long = "trace-tag", global = true)]
    pub tags: Vec<String>,
    /// Langfuse trace metadata as `key=value`; repeatable.
    #[arg(long = "trace-metadata", global = true, value_parser = parse_key_value)]
    pub metadata: Vec<(String, String)>,
    /// Langfuse session id (default: the Loop session id).
    #[arg(long = "trace-session", global = true)]
    pub session: Option<String>,
    /// Langfuse environment, e.g. `benchmark`.
    #[arg(long = "trace-env", global = true)]
    pub environment: Option<String>,
    /// W3C traceparent of an enclosing trace, so a runner can own the parent span.
    #[arg(long = "trace-parent", global = true, env = "TRACEPARENT")]
    pub parent: Option<String>,
}

#[cfg(feature = "telemetry")]
impl TraceArgs {
    /// Trace attributes for the root span.
    pub fn trace_attrs(&self, default_session: &str) -> TraceAttrs {
        TraceAttrs {
            name: self.name.clone(),
            session_id: Some(
                self.session
                    .clone()
                    .unwrap_or_else(|| default_session.to_string()),
            ),
            user_id: None,
            tags: self.tags.clone(),
            metadata: self.metadata.iter().cloned().collect::<BTreeMap<_, _>>(),
            environment: self.environment.clone(),
            release: Some(env!("CARGO_PKG_VERSION").to_string()),
        }
    }
}

#[cfg(feature = "telemetry")]
fn parse_key_value(raw: &str) -> Result<(String, String), String> {
    match raw.split_once('=') {
        Some((key, value)) if !key.trim().is_empty() => {
            Ok((key.trim().to_string(), value.to_string()))
        }
        _ => Err(format!("expected key=value, got `{raw}`")),
    }
}

/// Outcome of a traced print run.
#[derive(Debug)]
pub struct PrintOutcome {
    /// Final assistant text, or why the run failed.
    pub result: anyhow::Result<String>,
    /// Trace id when the run was exported.
    pub trace_id: Option<String>,
}

/// Run `prompt` through `run`; untraced without the `telemetry` feature.
#[cfg(not(feature = "telemetry"))]
pub async fn traced_print<F, Fut>(
    _prompt: &str,
    _trace: &TraceArgs,
    _default_session: &str,
    run: F,
) -> PrintOutcome
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<String>>,
{
    PrintOutcome {
        result: run().await,
        trace_id: None,
    }
}

/// Run `prompt` through `run` inside the `loop.print` root observation.
#[cfg(feature = "telemetry")]
pub async fn traced_print<F, Fut>(
    prompt: &str,
    trace: &TraceArgs,
    default_session: &str,
    run: F,
) -> PrintOutcome
where
    F: FnOnce() -> Fut,
    Fut: Future<Output = anyhow::Result<String>>,
{
    let root = obs::span(ROOT_SPAN_NAME);
    if let Some(parent) = &trace.parent {
        if !root.continue_trace(parent) {
            tracing::warn!(target: "loop_cli", traceparent = %parent, "ignoring invalid traceparent");
        }
    }
    root.record_trace(&trace.trace_attrs(default_session));
    root.record_input(prompt);

    let result = run().instrument(root.clone()).await;
    match &result {
        Ok(text) => root.record_output(text),
        Err(err) => root.record_error(&format!("{err:#}")),
    }
    PrintOutcome {
        result,
        trace_id: root.trace_id(),
    }
}

/// `loop --print`: run one prompt, print the answer, report the trace id on stderr.
pub async fn run_print(
    runtime: &mut CliRuntime,
    prompt: String,
    trace: &TraceArgs,
) -> anyhow::Result<()> {
    // No one can approve anything here: external agents get the default
    // host, which rejects every request, so they decline the action.
    match runtime.pending_agent.take() {
        Some(PendingAgent::New { id, model }) => runtime.use_agent(&id, model.as_deref()).await?,
        Some(PendingAgent::Resume(record)) => runtime.resume_agent(&record).await?,
        None => {}
    }
    let backend = runtime.backend.clone();
    let session = runtime.session_ref();
    let input = prompt.clone();
    let outcome = traced_print(&input, trace, &runtime.session_id, || async move {
        print_turn(backend.as_ref(), &session, prompt).await
    })
    .await;
    if let Some(trace_id) = &outcome.trace_id {
        eprintln!("{TRACE_ID_PREFIX}{trace_id}");
    }
    let text = outcome.result?;
    println!("{text}");
    Ok(())
}

/// Flush and stop exporting before the process exits; reports a failed last export.
#[cfg(feature = "telemetry")]
pub async fn shutdown_telemetry(telemetry: &TelemetryHandle) {
    let handle = telemetry.clone();
    // Shutdown blocks on the exporter; keep it off the async workers.
    let _ = tokio::task::spawn_blocking(move || handle.shutdown()).await;
    if let Some(err) = telemetry.status().last_error {
        eprintln!("loop: trace export failed: {err}");
    }
}

/// Run one turn and return the text of the agent's last message, or why the
/// turn failed.
pub async fn print_turn(
    backend: &dyn AgentBackend,
    session: &SessionRef,
    prompt: String,
) -> anyhow::Result<String> {
    // Subscribe before prompting so no update of this turn is missed.
    let mut updates = backend.subscribe();
    let turn = backend
        .prompt(session, vec![ContentBlock::text(prompt)])
        .await?;
    let mut last_text = String::new();
    loop {
        let envelope = match updates.recv().await {
            Ok(envelope) => envelope,
            Err(RecvError::Lagged(missed)) => {
                anyhow::bail!("missed {missed} updates from the agent")
            }
            Err(RecvError::Closed) => break,
        };
        if envelope.session != session.key {
            continue;
        }
        match envelope.update {
            SessionUpdate::MessageStarted {
                role: Role::Agent, ..
            } => last_text.clear(),
            SessionUpdate::MessageChunk {
                role: Role::Agent,
                content,
                ..
            } => {
                if let Some(text) = content.as_text() {
                    last_text.push_str(text);
                }
            }
            SessionUpdate::TurnEnded { stop, error, .. } => {
                if let Some(error) = error {
                    anyhow::bail!("{}", error.message);
                }
                if stop == StopReason::Cancelled {
                    anyhow::bail!("the turn was cancelled");
                }
                return Ok(last_text);
            }
            _ => {}
        }
    }
    // The stream closed without a turn end; the turn's outcome decides.
    match turn.outcome().await {
        TurnOutcome::Completed(_) => Ok(last_text),
        TurnOutcome::Cancelled => anyhow::bail!("the turn was cancelled"),
        TurnOutcome::Failed(error) => anyhow::bail!("{}", error.message),
        TurnOutcome::Uncertain => {
            anyhow::bail!("lost contact with the agent; the turn may be incomplete")
        }
    }
}

/// The assistant's text, or an error when the run stopped on error / abort.
pub fn final_text(message: &AgentMessage) -> anyhow::Result<String> {
    let Some(Message::Assistant(assistant)) = message.as_llm() else {
        return Ok(format!("{message:?}"));
    };
    if let Some(failure) = assistant.failure() {
        anyhow::bail!("{failure}");
    }
    Ok(assistant
        .content
        .iter()
        .filter_map(|block| match block {
            AssistantContent::Text(t) => Some(t.text.as_str()),
            _ => None,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    #[cfg(feature = "telemetry")]
    use clap::Parser;
    use loop_ai::StopReason;

    use super::*;

    #[cfg(feature = "telemetry")]
    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        trace: TraceArgs,
    }

    #[cfg(feature = "telemetry")]
    fn parse(args: &[&str]) -> Result<TraceArgs, clap::Error> {
        Cli::try_parse_from(std::iter::once("loop").chain(args.iter().copied())).map(|c| c.trace)
    }

    #[cfg(feature = "telemetry")]
    #[test]
    fn trace_flags_map_to_trace_attrs() {
        let args = parse(&[
            "--trace-name",
            "swe-bench",
            "--trace-tag",
            "bench",
            "--trace-tag",
            "nightly",
            "--trace-metadata",
            "task_id=django-123",
            "--trace-metadata",
            "run=r1=a",
            "--trace-env",
            "benchmark",
        ])
        .unwrap();
        let attrs = args.trace_attrs("sess-default");
        assert_eq!(attrs.name.as_deref(), Some("swe-bench"));
        assert_eq!(attrs.tags, vec!["bench", "nightly"]);
        assert_eq!(attrs.metadata["task_id"], "django-123");
        assert_eq!(attrs.metadata["run"], "r1=a");
        assert_eq!(attrs.environment.as_deref(), Some("benchmark"));
        assert_eq!(attrs.session_id.as_deref(), Some("sess-default"));
        assert_eq!(attrs.release.as_deref(), Some(env!("CARGO_PKG_VERSION")));
    }

    #[cfg(feature = "telemetry")]
    #[test]
    fn trace_session_overrides_default() {
        let attrs = parse(&["--trace-session", "run-7"])
            .unwrap()
            .trace_attrs("sess");
        assert_eq!(attrs.session_id.as_deref(), Some("run-7"));
    }

    #[cfg(feature = "telemetry")]
    #[test]
    fn malformed_metadata_is_rejected() {
        assert!(parse(&["--trace-metadata", "novalue"]).is_err());
        assert!(parse(&["--trace-metadata", "=x"]).is_err());
    }

    #[test]
    fn final_text_joins_text_blocks_and_fails_on_error() {
        let mut assistant = loop_ai::AssistantMessage {
            content: vec![
                AssistantContent::Text(loop_ai::TextContent {
                    text: "hel".into(),
                    text_signature: None,
                }),
                AssistantContent::Text(loop_ai::TextContent {
                    text: "lo".into(),
                    text_signature: None,
                }),
            ],
            api: "faux".into(),
            provider: "faux".into(),
            model: "m".into(),
            response_model: None,
            response_id: None,
            usage: Default::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            raw_stop_reason: None,
            timestamp: 0,
        };
        assert_eq!(
            final_text(&AgentMessage::assistant(assistant.clone())).unwrap(),
            "hello"
        );
        assistant.stop_reason = StopReason::Error;
        assistant.error_message = Some("rate limited".into());
        let err = final_text(&AgentMessage::assistant(assistant)).unwrap_err();
        assert_eq!(err.to_string(), "rate limited");
    }
}
