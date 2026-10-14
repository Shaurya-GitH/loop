//! `--print` tracing end to end: faux provider → agent loop → in-memory exporter.

use std::sync::Arc;

use loop_agent::{
    run_agent_loop, stream_fn_from_models, AgentContext, AgentLoopConfig, AgentMessage,
};
use loop_ai::providers::{faux_provider, FauxResponse, FauxScript};
use loop_ai::Models;
use loop_cli::print_mode::{final_text, traced_print, TraceArgs, ROOT_SPAN_NAME};
use loop_telemetry::obs::keys;
use loop_telemetry::{CredentialSource, TelemetryHandle};
use opentelemetry_sdk::trace::{InMemorySpanExporter, SpanData};
use tracing_subscriber::layer::SubscriberExt;

struct Capture {
    handle: TelemetryHandle,
    exporter: InMemorySpanExporter,
    _guard: tracing::subscriber::DefaultGuard,
}

impl Capture {
    fn new() -> Self {
        let handle = TelemetryHandle::new("test", true);
        let exporter = InMemorySpanExporter::default();
        handle.install_exporter(exporter.clone(), "memory".into(), CredentialSource::Env);
        let guard =
            tracing::subscriber::set_default(tracing_subscriber::registry().with(handle.layer()));
        Self {
            handle,
            exporter,
            _guard: guard,
        }
    }

    fn span(&self, name: &str) -> SpanData {
        self.handle.flush();
        self.exporter
            .get_finished_spans()
            .unwrap()
            .into_iter()
            .find(|s| s.name == name)
            .unwrap_or_else(|| panic!("no `{name}` span"))
    }
}

fn attr(span: &SpanData, key: &str) -> Option<String> {
    span.attributes
        .iter()
        .find(|kv| kv.key.as_str() == key)
        .map(|kv| kv.value.as_str().into_owned())
}

async fn agent_run(response: FauxResponse) -> anyhow::Result<String> {
    let script = FauxScript::new();
    script.push(response);
    let models = Models::new();
    models.set_provider(faux_provider(script));
    let model = models.get_model("faux", "faux-model").unwrap();
    let messages = run_agent_loop(
        vec![AgentMessage::user_text("hi")],
        AgentContext::default(),
        AgentLoopConfig::new(model),
        Arc::new(|_| Box::pin(async {})),
        None,
        Some(stream_fn_from_models(Arc::new(models))),
    )
    .await?;
    final_text(&messages.last().cloned().unwrap())
}

#[tokio::test]
async fn print_run_is_one_trace_rooted_at_loop_print() {
    let capture = Capture::new();
    let trace = TraceArgs {
        tags: vec!["bench".into()],
        metadata: vec![("task_id".into(), "t-1".into())],
        environment: Some("benchmark".into()),
        ..Default::default()
    };
    let outcome = traced_print("hi", &trace, "sess-1", || {
        agent_run(FauxResponse::Text("hello".into()))
    })
    .await;
    assert_eq!(outcome.result.unwrap(), "hello");

    let root = capture.span(ROOT_SPAN_NAME);
    let run = capture.span("loop.run");
    assert_eq!(
        outcome.trace_id.as_deref(),
        Some(root.span_context.trace_id().to_string().as_str())
    );
    assert_eq!(run.parent_span_id, root.span_context.span_id());
    assert_eq!(run.span_context.trace_id(), root.span_context.trace_id());

    assert_eq!(attr(&root, keys::SESSION_ID).as_deref(), Some("sess-1"));
    assert_eq!(attr(&root, keys::ENVIRONMENT).as_deref(), Some("benchmark"));
    assert_eq!(
        attr(&root, &format!("{}task_id", keys::TRACE_METADATA_PREFIX)).as_deref(),
        Some("t-1")
    );
    assert_eq!(attr(&root, keys::INPUT).as_deref(), Some("hi"));
    assert_eq!(attr(&root, keys::OUTPUT).as_deref(), Some("hello"));
    // The enclosing root owns trace attributes; the nested run does not repeat them.
    assert!(attr(&run, keys::SESSION_ID).is_none());
}

#[tokio::test]
async fn traceparent_places_run_under_callers_trace() {
    let capture = Capture::new();
    let trace_hex = "4bf92f3577b34da6a3ce929d0e0e4736";
    let trace = TraceArgs {
        parent: Some(format!("00-{trace_hex}-00f067aa0ba902b7-01")),
        ..Default::default()
    };
    let outcome = traced_print("hi", &trace, "s", || {
        agent_run(FauxResponse::Text("x".into()))
    })
    .await;
    assert_eq!(outcome.trace_id.as_deref(), Some(trace_hex));
    assert_eq!(
        capture.span("loop.run").span_context.trace_id().to_string(),
        trace_hex
    );
}

#[tokio::test]
async fn failed_run_marks_root_error_and_returns_err() {
    let capture = Capture::new();
    let outcome = traced_print("hi", &TraceArgs::default(), "s", || {
        agent_run(FauxResponse::Error("rate limited".into()))
    })
    .await;
    assert_eq!(outcome.result.unwrap_err().to_string(), "rate limited");
    let root = capture.span(ROOT_SPAN_NAME);
    assert_eq!(attr(&root, keys::LEVEL).as_deref(), Some("ERROR"));
    assert_eq!(
        attr(&root, keys::STATUS_MESSAGE).as_deref(),
        Some("rate limited")
    );
}
