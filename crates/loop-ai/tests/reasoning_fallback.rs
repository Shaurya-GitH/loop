//! A server that rejects reasoning parameters gets the request again without
//! them, and the model is remembered so later requests skip them.

use futures::StreamExt;
use loop_ai::providers::{custom_provider, CustomModelSpec, CustomProviderConfig};
use loop_ai::{
    AssistantMessageEvent, Context, Message, Models, SimpleStreamOptions, ThinkingLevel,
};
use loop_test_support::{FakeHttpServer, FakeResponse, RecordedRequest};

const REJECTED: &str = r#"{"error":{"message":"custom_openai does not support parameters: ['reasoning_effort'], for model=m. To drop these, set `litellm.drop_params=True`","code":"400"}}"#;
const ANSWER: &str = "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"ok\"}}]}\n\ndata: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";

fn route(request: &RecordedRequest) -> FakeResponse {
    let body = String::from_utf8_lossy(&request.body);
    if body.contains("reasoning_effort") {
        FakeResponse::new(400, "application/json", REJECTED)
    } else {
        FakeResponse::new(200, "text/event-stream", ANSWER)
    }
}

async fn reply(models: &Models, model: &loop_ai::Model) -> Result<String, String> {
    let context = Context {
        messages: vec![Message::user_text("hi")],
        ..Default::default()
    };
    let options = SimpleStreamOptions::with_reasoning(ThinkingLevel::High);
    let mut stream = models.stream_simple(model, &context, options);
    let mut text = String::new();
    while let Some(event) = stream.next().await {
        match event {
            AssistantMessageEvent::TextDelta { delta, .. } => text.push_str(&delta),
            AssistantMessageEvent::Error { error, .. } => {
                return Err(error.error_message.unwrap_or_default())
            }
            _ => {}
        }
    }
    Ok(text)
}

#[tokio::test]
async fn rejected_reasoning_is_dropped_and_remembered() {
    let server = FakeHttpServer::start(route);
    let models = Models::new();
    let mut spec = CustomModelSpec::new("m");
    spec.reasoning = true;
    models.set_provider(custom_provider(CustomProviderConfig {
        id: "fallback-test".into(),
        name: None,
        base_url: server.base_url().into(),
        api_key_env: Vec::new(),
        models: vec![spec],
        headers: None,
    }));
    let model = models.get_model("fallback-test", "m").unwrap();
    assert!(!loop_ai::reasoning_rejected(&model));

    assert_eq!(reply(&models, &model).await.unwrap(), "ok");
    assert!(loop_ai::reasoning_rejected(&model));
    let first: Vec<_> = server.requests();
    assert_eq!(first.len(), 2, "the rejected request, then the retry");

    // Later requests go straight through without reasoning parameters.
    assert_eq!(reply(&models, &model).await.unwrap(), "ok");
    let all = server.requests();
    assert_eq!(all.len(), 3);
    assert!(!String::from_utf8_lossy(&all[2].body).contains("reasoning_effort"));
}
