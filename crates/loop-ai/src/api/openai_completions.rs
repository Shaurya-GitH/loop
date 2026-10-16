//! OpenAI Chat Completions (`/v1/chat/completions`) wire adapter.

use std::collections::HashMap;
use std::sync::Arc;

use eventsource_stream::Eventsource;
use futures::StreamExt;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use serde_json::{json, Value};

use crate::api::detect_compat::resolve_compat;
use crate::api::transform_messages::transform_messages;
use crate::auth::ModelAuth;
use crate::models::ApiAdapter;
use crate::stream::{create_assistant_message_event_stream, AssistantMessageEventStream};
use crate::types::{
    AssistantContent, AssistantMessage, AssistantMessageEvent, Context, MaxTokensField, Message,
    Model, ProviderResponse, ResolvedOpenAICompletionsCompat, SimpleStreamOptions, StopReason,
    StreamOptions, TextContent, ThinkingContent, ThinkingFormat, ThinkingLevel, ToolCall,
    ToolResultContent, Usage, UserContent, UserMessageContent, API_OPENAI_COMPLETIONS,
};
use crate::utils::{calculate_cost, parse_streaming_json};

/// Adapter for OpenAI-compatible Chat Completions APIs.
#[derive(Debug, Clone)]
pub struct OpenAICompletionsAdapter {
    client: reqwest::Client,
}

impl Default for OpenAICompletionsAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl OpenAICompletionsAdapter {
    /// Create with a streaming-optimised reqwest client (connect timeout, no overall timeout).
    pub fn new() -> Self {
        Self {
            client: super::http::streaming_http_client(),
        }
    }

    /// Create with a shared client.
    pub fn with_client(client: reqwest::Client) -> Self {
        Self { client }
    }

    /// Shared adapter handle.
    pub fn shared() -> Arc<Self> {
        Arc::new(Self::new())
    }
}

impl ApiAdapter for OpenAICompletionsAdapter {
    fn stream(
        &self,
        model: &Model,
        context: &Context,
        options: StreamOptions,
        auth: ModelAuth,
    ) -> AssistantMessageEventStream {
        self.stream_inner(model, context, options, auth, None)
    }

    fn stream_simple(
        &self,
        model: &Model,
        context: &Context,
        options: SimpleStreamOptions,
        auth: ModelAuth,
    ) -> AssistantMessageEventStream {
        let reasoning = options.reasoning;
        let mut base = options.base;
        // Clamp max tokens against context estimate if unset — light touch.
        if base.max_tokens.is_none() {
            base.max_tokens = Some(model.max_tokens.min(u64::from(u32::MAX)) as u32);
        }
        self.stream_inner(model, context, base, auth, reasoning)
    }
}

impl OpenAICompletionsAdapter {
    fn stream_inner(
        &self,
        model: &Model,
        context: &Context,
        options: StreamOptions,
        auth: ModelAuth,
        reasoning: Option<ThinkingLevel>,
    ) -> AssistantMessageEventStream {
        let stream = create_assistant_message_event_stream();
        let handle = stream.handle();
        let client = self.client.clone();
        let model = model.clone();
        let context = context.clone();

        tokio::spawn(async move {
            if let Err(err) = run_stream(
                client,
                model.clone(),
                context,
                options,
                auth,
                reasoning,
                handle.clone(),
            )
            .await
            {
                let mut msg = AssistantMessage::pending(&model);
                let reason = if err.aborted {
                    StopReason::Aborted
                } else {
                    StopReason::Error
                };
                msg.stop_reason = reason;
                msg.error_message = Some(err.message);
                handle.push(AssistantMessageEvent::Error { reason, error: msg });
            }
        });

        stream
    }
}

struct StreamFail {
    message: String,
    aborted: bool,
}

/// Send one chat completions request, bounded by the time-to-first-byte
/// limit and the caller's cancellation.
async fn send_request(
    client: &reqwest::Client,
    url: &str,
    headers: HeaderMap,
    payload: &Value,
    options: &StreamOptions,
) -> Result<reqwest::Response, StreamFail> {
    let req = client.post(url).headers(headers).json(payload);
    // Do not set an overall reqwest timeout here — it would abort long SSE bodies.
    // Bound only time-to-first-byte (headers) via `response_header_timeout_ms`.

    let cancel = options.cancel.clone();
    let response_fut = req.send();
    // None => default 60s; Some(0) => no limit; Some(n) => n ms.
    const DEFAULT_TTFB_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
    let ttfb: Option<std::time::Duration> = match options.response_header_timeout_ms {
        None => Some(DEFAULT_TTFB_TIMEOUT),
        Some(0) => None,
        Some(ms) => Some(std::time::Duration::from_millis(ms)),
    };

    match (cancel.as_ref(), ttfb) {
        (Some(token), Some(bound)) => {
            tokio::select! {
                _ = token.cancelled() => {
                    return Err(StreamFail { message: "aborted".into(), aborted: true });
                }
                res = tokio::time::timeout(bound, response_fut) => {
                    match res {
                        Ok(inner) => inner,
                        Err(_) => {
                            return Err(StreamFail {
                                message: format!(
                                    "timed out waiting for model response ({}s); set responseHeaderTimeoutMs to 0 for long-running waits",
                                    bound.as_secs()
                                ),
                                aborted: false,
                            });
                        }
                    }
                }
            }
        }
        (Some(token), None) => {
            tokio::select! {
                _ = token.cancelled() => {
                    return Err(StreamFail { message: "aborted".into(), aborted: true });
                }
                res = response_fut => res,
            }
        }
        (None, Some(bound)) => match tokio::time::timeout(bound, response_fut).await {
            Ok(inner) => inner,
            Err(_) => {
                return Err(StreamFail {
                    message: format!(
                        "timed out waiting for model response ({}s); set responseHeaderTimeoutMs to 0 for long-running waits",
                        bound.as_secs()
                    ),
                    aborted: false,
                });
            }
        },
        (None, None) => response_fut.await,
    }
    .map_err(|e| StreamFail {
        message: e.to_string(),
        aborted: false,
    })
}

async fn run_stream(
    client: reqwest::Client,
    model: Model,
    context: Context,
    options: StreamOptions,
    auth: ModelAuth,
    reasoning: Option<ThinkingLevel>,
    handle: crate::stream::AssistantMessageEventStreamHandle,
) -> Result<(), StreamFail> {
    if let Some(token) = &options.cancel {
        if token.is_cancelled() {
            return Err(StreamFail {
                message: "aborted".into(),
                aborted: true,
            });
        }
    }

    let compat = resolve_compat(&model.base_url, model.compat.as_ref());
    let base_url = auth
        .base_url
        .as_deref()
        .unwrap_or(model.base_url.as_str())
        .trim_end_matches('/');
    let url = format!("{base_url}/chat/completions");

    let messages = transform_messages(&context.messages, &model);
    let mut payload = build_payload(&model, &context, &messages, &options, &compat, reasoning)?;

    if let Some(cb) = &options.on_payload {
        if let Some(replaced) = cb(&payload, &model) {
            payload = replaced;
        }
    }

    let mut headers = HeaderMap::new();
    headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    if let Some(key) = auth.api_key.as_deref().or(options.api_key.as_deref()) {
        if !key.is_empty() {
            let value = format!("Bearer {key}");
            headers.insert(
                AUTHORIZATION,
                HeaderValue::from_str(&value).map_err(|e| StreamFail {
                    message: format!("invalid api key header: {e}"),
                    aborted: false,
                })?,
            );
        }
    }
    if let Some(hdrs) = &options.headers {
        for (k, v) in hdrs {
            if let Some(val) = v {
                if let (Ok(name), Ok(value)) = (
                    HeaderName::from_bytes(k.as_bytes()),
                    HeaderValue::from_str(val),
                ) {
                    headers.insert(name, value);
                }
            }
        }
    }

    let cancel = options.cancel.clone();
    let mut response = send_request(&client, &url, headers.clone(), &payload, &options).await?;
    // Some OpenAI-compatible servers reject reasoning parameters for models
    // they serve. Retry once without them and remember the model.
    if reasoning.is_some() && matches!(response.status().as_u16(), 400 | 422) {
        let body = response.text().await.unwrap_or_default();
        if !rejects_reasoning(&body) {
            return Err(StreamFail {
                message: crate::utils::summarize_http_error(400, &body),
                aborted: false,
            });
        }
        tracing::warn!(model = %model.id, "the provider rejected reasoning parameters; retrying without them");
        mark_reasoning_rejected(&model);
        payload = build_payload(&model, &context, &messages, &options, &compat, None)?;
        if let Some(cb) = &options.on_payload {
            if let Some(replaced) = cb(&payload, &model) {
                payload = replaced;
            }
        }
        response = send_request(&client, &url, headers, &payload, &options).await?;
    }

    let status = response.status();
    let mut resp_headers = HashMap::new();
    for (k, v) in response.headers() {
        if let Ok(val) = v.to_str() {
            resp_headers.insert(k.as_str().to_string(), val.to_string());
        }
    }
    if let Some(cb) = &options.on_response {
        cb(
            &ProviderResponse {
                status: status.as_u16(),
                headers: resp_headers,
            },
            &model,
        );
    }

    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(StreamFail {
            message: crate::utils::summarize_http_error(status.as_u16(), &body),
            aborted: false,
        });
    }

    let mut partial = AssistantMessage::pending(&model);
    handle.push(AssistantMessageEvent::Start {
        partial: partial.clone(),
    });

    // Track open content blocks: index in partial.content
    let mut text_index: Option<usize> = None;
    let mut thinking_index: Option<usize> = None;
    // tool index in stream -> content index
    let mut tool_map: HashMap<u32, usize> = HashMap::new();
    let mut tool_args: HashMap<usize, String> = HashMap::new();
    let mut finish_reason: Option<String> = None;

    let byte_stream = response.bytes_stream();
    let mut event_stream = byte_stream.eventsource();

    // If no SSE chunk arrives within this window, treat the connection as stalled.
    const SSE_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

    loop {
        let next = if let Some(token) = &cancel {
            tokio::select! {
                _ = token.cancelled() => {
                    partial.stop_reason = StopReason::Aborted;
                    partial.error_message = Some("aborted".into());
                    close_open_blocks(&mut partial, &mut text_index, &mut thinking_index, &mut tool_map, &mut tool_args, &handle);
                    handle.push(AssistantMessageEvent::Error {
                        reason: StopReason::Aborted,
                        error: partial,
                    });
                    return Ok(());
                }
                ev = tokio::time::timeout(SSE_IDLE_TIMEOUT, event_stream.next()) => {
                    match ev {
                        Ok(inner) => inner,
                        Err(_) => {
                            return Err(StreamFail {
                                message: format!("server stopped responding (no data for {}s)", SSE_IDLE_TIMEOUT.as_secs()),
                                aborted: false,
                            });
                        }
                    }
                }
            }
        } else {
            match tokio::time::timeout(SSE_IDLE_TIMEOUT, event_stream.next()).await {
                Ok(inner) => inner,
                Err(_) => {
                    return Err(StreamFail {
                        message: format!(
                            "server stopped responding (no data for {}s)",
                            SSE_IDLE_TIMEOUT.as_secs()
                        ),
                        aborted: false,
                    });
                }
            }
        };

        let Some(event) = next else { break };
        let event = event.map_err(|e| StreamFail {
            message: format!("sse error: {e}"),
            aborted: false,
        })?;

        let data = event.data.trim();
        if data.is_empty() {
            continue;
        }
        if data == "[DONE]" {
            break;
        }

        let chunk: Value = match serde_json::from_str(data) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(error = %e, "skipping unparseable SSE chunk");
                continue;
            }
        };

        if let Some(id) = chunk.get("id").and_then(|v| v.as_str()) {
            partial.response_id = Some(id.to_string());
        }
        if let Some(m) = chunk.get("model").and_then(|v| v.as_str()) {
            partial.response_model = Some(m.to_string());
        }

        if let Some(usage) = chunk.get("usage") {
            apply_usage(&mut partial.usage, usage);
        }

        let choices = chunk
            .get("choices")
            .and_then(|c| c.as_array())
            .cloned()
            .unwrap_or_default();

        for choice in choices {
            if let Some(fr) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                if !fr.is_empty() && fr != "null" {
                    finish_reason = Some(fr.to_string());
                }
            }

            let delta = choice.get("delta").cloned().unwrap_or(json!({}));

            // Reasoning / thinking content (various vendor shapes).
            if let Some(reasoning_text) = extract_reasoning_delta(&delta) {
                if !reasoning_text.is_empty() {
                    if thinking_index.is_none() {
                        let idx = partial.content.len();
                        partial
                            .content
                            .push(AssistantContent::Thinking(ThinkingContent {
                                thinking: String::new(),
                                thinking_signature: None,
                                redacted: None,
                            }));
                        thinking_index = Some(idx);
                        handle.push(AssistantMessageEvent::ThinkingStart {
                            content_index: idx,
                            partial: partial.clone(),
                        });
                    }
                    if let Some(idx) = thinking_index {
                        if let AssistantContent::Thinking(t) = &mut partial.content[idx] {
                            t.thinking.push_str(&reasoning_text);
                        }
                        handle.push(AssistantMessageEvent::ThinkingDelta {
                            content_index: idx,
                            delta: reasoning_text,
                            partial: partial.clone(),
                        });
                    }
                }
            }

            if let Some(content) = delta.get("content").and_then(|v| v.as_str()) {
                if !content.is_empty() {
                    // Close thinking before text if needed.
                    if let Some(idx) = thinking_index.take() {
                        let text = thinking_text(&partial, idx);
                        handle.push(AssistantMessageEvent::ThinkingEnd {
                            content_index: idx,
                            content: text,
                            partial: partial.clone(),
                        });
                    }
                    if text_index.is_none() {
                        let idx = partial.content.len();
                        partial.content.push(AssistantContent::Text(TextContent {
                            text: String::new(),
                            text_signature: None,
                        }));
                        text_index = Some(idx);
                        handle.push(AssistantMessageEvent::TextStart {
                            content_index: idx,
                            partial: partial.clone(),
                        });
                    }
                    if let Some(idx) = text_index {
                        if let AssistantContent::Text(t) = &mut partial.content[idx] {
                            t.text.push_str(content);
                        }
                        handle.push(AssistantMessageEvent::TextDelta {
                            content_index: idx,
                            delta: content.to_string(),
                            partial: partial.clone(),
                        });
                    }
                }
            }

            if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                // Close text/thinking before tools.
                if let Some(idx) = text_index.take() {
                    let text = text_content(&partial, idx);
                    handle.push(AssistantMessageEvent::TextEnd {
                        content_index: idx,
                        content: text,
                        partial: partial.clone(),
                    });
                }
                if let Some(idx) = thinking_index.take() {
                    let text = thinking_text(&partial, idx);
                    handle.push(AssistantMessageEvent::ThinkingEnd {
                        content_index: idx,
                        content: text,
                        partial: partial.clone(),
                    });
                }

                for tc in tool_calls {
                    let index = tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
                    let content_index = if let Some(&ci) = tool_map.get(&index) {
                        ci
                    } else {
                        let idx = partial.content.len();
                        let id = tc
                            .get("id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        let name = tc
                            .pointer("/function/name")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        partial.content.push(AssistantContent::ToolCall(ToolCall {
                            id,
                            name,
                            arguments: json!({}),
                            thought_signature: None,
                        }));
                        tool_map.insert(index, idx);
                        tool_args.insert(idx, String::new());
                        handle.push(AssistantMessageEvent::ToolcallStart {
                            content_index: idx,
                            partial: partial.clone(),
                        });
                        idx
                    };

                    // Update id/name if present on later chunks.
                    if let AssistantContent::ToolCall(call) = &mut partial.content[content_index] {
                        if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                            if !id.is_empty() {
                                call.id = id.to_string();
                            }
                        }
                        if let Some(name) = tc.pointer("/function/name").and_then(|v| v.as_str()) {
                            if !name.is_empty() {
                                call.name = name.to_string();
                            }
                        }
                        if let Some(args) =
                            tc.pointer("/function/arguments").and_then(|v| v.as_str())
                        {
                            if let Some(buf) = tool_args.get_mut(&content_index) {
                                buf.push_str(args);
                                call.arguments = parse_streaming_json(buf);
                            }
                            handle.push(AssistantMessageEvent::ToolcallDelta {
                                content_index,
                                delta: args.to_string(),
                                partial: partial.clone(),
                            });
                        }
                    }
                }
            }
        }
    }

    // Close open blocks.
    if let Some(idx) = text_index.take() {
        let text = text_content(&partial, idx);
        handle.push(AssistantMessageEvent::TextEnd {
            content_index: idx,
            content: text,
            partial: partial.clone(),
        });
    }
    if let Some(idx) = thinking_index.take() {
        let text = thinking_text(&partial, idx);
        handle.push(AssistantMessageEvent::ThinkingEnd {
            content_index: idx,
            content: text,
            partial: partial.clone(),
        });
    }
    for (_stream_idx, content_index) in tool_map {
        if let AssistantContent::ToolCall(call) = &partial.content[content_index] {
            let finalized = {
                let raw = tool_args.get(&content_index).cloned().unwrap_or_default();
                let mut c = call.clone();
                if let Ok(v) = serde_json::from_str::<Value>(&raw) {
                    c.arguments = v;
                } else {
                    c.arguments = parse_streaming_json(&raw);
                }
                c
            };
            if let AssistantContent::ToolCall(call) = &mut partial.content[content_index] {
                *call = finalized.clone();
            }
            handle.push(AssistantMessageEvent::ToolcallEnd {
                content_index,
                tool_call: finalized,
                partial: partial.clone(),
            });
        }
    }

    calculate_cost(&model, &mut partial.usage);
    if partial.usage.total_tokens == 0 {
        partial.usage.total_tokens = partial.usage.input + partial.usage.output;
    }

    let reason = map_finish_reason(finish_reason.as_deref(), &partial, &compat);
    partial.stop_reason = reason;
    partial.raw_stop_reason = finish_reason;
    partial.api = API_OPENAI_COMPLETIONS.to_string();

    if reason.is_error() {
        handle.push(AssistantMessageEvent::Error {
            reason,
            error: partial,
        });
    } else {
        handle.push(AssistantMessageEvent::Done {
            reason,
            message: partial,
        });
    }

    Ok(())
}

fn close_open_blocks(
    partial: &mut AssistantMessage,
    text_index: &mut Option<usize>,
    thinking_index: &mut Option<usize>,
    tool_map: &mut HashMap<u32, usize>,
    tool_args: &mut HashMap<usize, String>,
    handle: &crate::stream::AssistantMessageEventStreamHandle,
) {
    if let Some(idx) = text_index.take() {
        let text = text_content(partial, idx);
        handle.push(AssistantMessageEvent::TextEnd {
            content_index: idx,
            content: text,
            partial: partial.clone(),
        });
    }
    if let Some(idx) = thinking_index.take() {
        let text = thinking_text(partial, idx);
        handle.push(AssistantMessageEvent::ThinkingEnd {
            content_index: idx,
            content: text,
            partial: partial.clone(),
        });
    }
    for (_k, content_index) in tool_map.drain() {
        if let AssistantContent::ToolCall(call) = &partial.content[content_index] {
            let mut finalized = call.clone();
            if let Some(raw) = tool_args.get(&content_index) {
                finalized.arguments = parse_streaming_json(raw);
            }
            handle.push(AssistantMessageEvent::ToolcallEnd {
                content_index,
                tool_call: finalized,
                partial: partial.clone(),
            });
        }
    }
}

fn text_content(partial: &AssistantMessage, idx: usize) -> String {
    match partial.content.get(idx) {
        Some(AssistantContent::Text(t)) => t.text.clone(),
        _ => String::new(),
    }
}

fn thinking_text(partial: &AssistantMessage, idx: usize) -> String {
    match partial.content.get(idx) {
        Some(AssistantContent::Thinking(t)) => t.thinking.clone(),
        _ => String::new(),
    }
}

fn extract_reasoning_delta(delta: &Value) -> Option<String> {
    if let Some(s) = delta.get("reasoning_content").and_then(|v| v.as_str()) {
        return Some(s.to_string());
    }
    if let Some(s) = delta.get("reasoning").and_then(|v| v.as_str()) {
        return Some(s.to_string());
    }
    if let Some(obj) = delta.get("reasoning").and_then(|v| v.as_object()) {
        if let Some(s) = obj.get("content").and_then(|v| v.as_str()) {
            return Some(s.to_string());
        }
    }
    None
}

fn apply_usage(usage: &mut Usage, value: &Value) {
    if let Some(v) = value.get("prompt_tokens").and_then(|v| v.as_u64()) {
        usage.input = v;
    }
    if let Some(v) = value.get("completion_tokens").and_then(|v| v.as_u64()) {
        usage.output = v;
    }
    if let Some(v) = value.get("total_tokens").and_then(|v| v.as_u64()) {
        usage.total_tokens = v;
    }
    if let Some(details) = value.get("prompt_tokens_details") {
        if let Some(v) = details.get("cached_tokens").and_then(|v| v.as_u64()) {
            usage.cache_read = v;
        }
    }
    if let Some(details) = value.get("completion_tokens_details") {
        if let Some(v) = details.get("reasoning_tokens").and_then(|v| v.as_u64()) {
            usage.reasoning = Some(v);
        }
    }
}

fn map_finish_reason(
    finish: Option<&str>,
    partial: &AssistantMessage,
    compat: &ResolvedOpenAICompletionsCompat,
) -> StopReason {
    let has_tools = partial
        .content
        .iter()
        .any(|c| matches!(c, AssistantContent::ToolCall(_)));

    match finish {
        Some("stop") | Some("end_turn") => StopReason::Stop,
        Some("length") | Some("max_tokens") => StopReason::Length,
        Some("tool_calls") | Some("function_call") => StopReason::ToolUse,
        Some("content_filter") => StopReason::Stop,
        Some(other) => partial_raw_fallback(other, has_tools),
        None if !compat.supports_finish_reason => {
            if has_tools {
                StopReason::ToolUse
            } else {
                StopReason::Stop
            }
        }
        None => {
            if has_tools {
                StopReason::ToolUse
            } else {
                StopReason::Stop
            }
        }
    }
}

fn partial_raw_fallback(raw: &str, has_tools: bool) -> StopReason {
    let lower = raw.to_lowercase();
    if lower.contains("tool") {
        StopReason::ToolUse
    } else if lower.contains("length") {
        StopReason::Length
    } else if has_tools {
        StopReason::ToolUse
    } else {
        StopReason::Stop
    }
}

fn build_payload(
    model: &Model,
    context: &Context,
    messages: &[Message],
    options: &StreamOptions,
    compat: &ResolvedOpenAICompletionsCompat,
    reasoning: Option<ThinkingLevel>,
) -> Result<Value, StreamFail> {
    let mut openai_messages = Vec::new();

    if let Some(system) = &context.system_prompt {
        let role = if compat.supports_developer_role {
            "developer"
        } else {
            "system"
        };
        openai_messages.push(json!({
            "role": role,
            "content": system,
        }));
    }

    for msg in messages {
        match convert_message(msg, compat) {
            Ok(Some(v)) => openai_messages.push(v),
            Ok(None) => {}
            Err(e) => {
                return Err(StreamFail {
                    message: e,
                    aborted: false,
                })
            }
        }
    }

    let mut payload = json!({
        "model": model.id,
        "messages": openai_messages,
        "stream": true,
    });

    if compat.supports_usage_in_streaming {
        payload["stream_options"] = json!({ "include_usage": true });
    }

    if let Some(temp) = options.temperature {
        payload["temperature"] = json!(temp);
    }

    if let Some(max) = options.max_tokens {
        match compat.max_tokens_field {
            MaxTokensField::MaxCompletionTokens => {
                payload["max_completion_tokens"] = json!(max);
            }
            MaxTokensField::MaxTokens => {
                payload["max_tokens"] = json!(max);
            }
        }
    }

    if compat.supports_store {
        payload["store"] = json!(false);
    }

    if let Some(tools) = &context.tools {
        if !tools.is_empty() || has_tool_history(messages) {
            let tool_defs: Vec<Value> = tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    })
                })
                .collect();
            payload["tools"] = Value::Array(tool_defs);
        }
    }

    apply_reasoning(&mut payload, model, compat, reasoning);

    let mut metadata = options
        .metadata
        .as_ref()
        .map(|m| {
            m.iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect::<serde_json::Map<String, Value>>()
        })
        .unwrap_or_default();
    if let Some(session_id) = &options.session_id {
        metadata.insert("session_id".into(), json!(session_id));
    }
    if !metadata.is_empty() {
        payload["metadata"] = Value::Object(metadata);
    }

    Ok(payload)
}

fn apply_reasoning(
    payload: &mut Value,
    model: &Model,
    compat: &ResolvedOpenAICompletionsCompat,
    reasoning: Option<ThinkingLevel>,
) {
    let Some(level) = reasoning else { return };
    if !model.reasoning && !compat.supports_reasoning_effort {
        return;
    }
    if reasoning_rejected(model) {
        return;
    }
    let effort = thinking_level_str(level);

    match compat.thinking_format {
        ThinkingFormat::Openai => {
            if compat.supports_reasoning_effort || model.reasoning {
                payload["reasoning_effort"] = json!(effort);
            }
        }
        ThinkingFormat::Openrouter => {
            payload["reasoning"] = json!({ "effort": effort });
        }
        ThinkingFormat::Deepseek => {
            payload["thinking"] = json!({ "type": "enabled" });
            if compat.supports_reasoning_effort {
                payload["reasoning_effort"] = json!(effort);
            }
        }
        ThinkingFormat::Together => {
            payload["reasoning"] = json!({ "enabled": true });
            if compat.supports_reasoning_effort {
                payload["reasoning_effort"] = json!(effort);
            }
        }
        ThinkingFormat::Zai => {
            payload["thinking"] = json!({ "type": "enabled" });
        }
        ThinkingFormat::Qwen => {
            payload["enable_thinking"] = json!(true);
        }
        ThinkingFormat::ChatTemplate | ThinkingFormat::QwenChatTemplate => {
            payload["chat_template_kwargs"] = json!({ "enable_thinking": true });
        }
        ThinkingFormat::StringThinking => {
            payload["thinking"] = json!(effort);
        }
        ThinkingFormat::AntLing => {
            payload["reasoning"] = json!({ "effort": effort });
        }
    }
}

/// Models whose server rejected reasoning parameters in this process, as
/// `provider/id`.
static REASONING_REJECTED: std::sync::Mutex<Vec<String>> = std::sync::Mutex::new(Vec::new());

fn model_key(model: &Model) -> String {
    format!("{}/{}", model.provider, model.id)
}

fn mark_reasoning_rejected(model: &Model) {
    let mut rejected = REASONING_REJECTED.lock().unwrap_or_else(|p| p.into_inner());
    let key = model_key(model);
    if !rejected.contains(&key) {
        rejected.push(key);
    }
}

/// Whether `model`'s server rejected reasoning parameters earlier in this
/// process. Requests for it are then sent without them.
pub fn reasoning_rejected(model: &Model) -> bool {
    REASONING_REJECTED
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .contains(&model_key(model))
}

/// Whether an error body says a reasoning parameter is not supported.
fn rejects_reasoning(body: &str) -> bool {
    let body = body.to_ascii_lowercase();
    let names_reasoning = [
        "reasoning_effort",
        "reasoning",
        "enable_thinking",
        "thinking",
    ]
    .iter()
    .any(|p| body.contains(p));
    let unsupported = [
        "not support",
        "unsupported",
        "unrecognized",
        "unknown parameter",
        "extra inputs are not permitted",
        "not allowed",
    ]
    .iter()
    .any(|p| body.contains(p));
    names_reasoning && unsupported
}

fn thinking_level_str(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::XHigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}

fn has_tool_history(messages: &[Message]) -> bool {
    messages.iter().any(|m| match m {
        Message::ToolResult(_) => true,
        Message::Assistant(a) => a
            .content
            .iter()
            .any(|c| matches!(c, AssistantContent::ToolCall(_))),
        _ => false,
    })
}

fn convert_message(
    msg: &Message,
    compat: &ResolvedOpenAICompletionsCompat,
) -> Result<Option<Value>, String> {
    match msg {
        Message::User(u) => {
            let content = match &u.content {
                UserMessageContent::Text(t) => json!(t),
                UserMessageContent::Blocks(blocks) => {
                    let parts: Vec<Value> = blocks
                        .iter()
                        .map(|b| match b {
                            UserContent::Text(t) => json!({"type":"text","text": t.text}),
                            UserContent::Image(img) => json!({
                                "type": "image_url",
                                "image_url": {
                                    "url": format!("data:{};base64,{}", img.mime_type, img.data)
                                }
                            }),
                        })
                        .collect();
                    Value::Array(parts)
                }
            };
            Ok(Some(json!({"role":"user","content": content})))
        }
        Message::Assistant(a) => {
            let mut text_parts = String::new();
            let mut tool_calls = Vec::new();
            for block in &a.content {
                match block {
                    AssistantContent::Text(t) => text_parts.push_str(&t.text),
                    AssistantContent::Thinking(t) => {
                        if compat.requires_thinking_as_text {
                            text_parts
                                .push_str(&format!("<thinking>\n{}\n</thinking>\n", t.thinking));
                        }
                        // else: omit thinking on replay unless same-model signatures retained upstream
                    }
                    AssistantContent::ToolCall(tc) => {
                        tool_calls.push(json!({
                            "id": tc.id,
                            "type": "function",
                            "function": {
                                "name": tc.name,
                                "arguments": tc.arguments.to_string(),
                            }
                        }));
                    }
                }
            }
            let mut out = json!({
                "role": "assistant",
                "content": if text_parts.is_empty() { Value::Null } else { json!(text_parts) },
            });
            if !tool_calls.is_empty() {
                out["tool_calls"] = Value::Array(tool_calls);
            }
            Ok(Some(out))
        }
        Message::ToolResult(t) => {
            let text = t
                .content
                .iter()
                .map(|c| match c {
                    ToolResultContent::Text(t) => t.text.as_str(),
                    ToolResultContent::Image(_) => "[image]",
                })
                .collect::<Vec<_>>()
                .join("\n");
            let mut out = json!({
                "role": "tool",
                "tool_call_id": t.tool_call_id,
                "content": text,
            });
            if compat.requires_tool_result_name {
                out["name"] = json!(t.tool_name);
            }
            Ok(Some(out))
        }
    }
}
