//! Implements the Anthropic provider adapter.
use async_stream::try_stream;
use base64::{engine::general_purpose::STANDARD, Engine};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde_json::{json, Value};

use crate::{
    config::ProviderConfig,
    model::{ContentPart, ModelInvocation, ProjectedContent, ProjectedMessage, Role, Usage},
    Error, Result,
};

use super::{
    attempt::{send_once, Attempt},
    map_sse_error, merge_extra_params, provider_event_error,
    recorder::recorded_headers,
    CallRecorder, FinishReason, ModelEvent, Provider, ProviderStream,
};

const DEFAULT_MAX_OUTPUT_TOKENS: u64 = 65_000;

pub struct AnthropicProvider {
    client: reqwest::Client,
    config: ProviderConfig,
    recorder: Option<CallRecorder>,
}

impl AnthropicProvider {
    pub fn new(client: reqwest::Client, config: ProviderConfig) -> Self {
        Self {
            client,
            config,
            recorder: None,
        }
    }

    pub fn with_recorder(mut self, recorder: Option<CallRecorder>) -> Self {
        self.recorder = recorder;
        self
    }
}

impl Provider for AnthropicProvider {
    fn stream(
        &self,
        invocation: ModelInvocation,
        cancellation: tokio_util::sync::CancellationToken,
    ) -> ProviderStream {
        let client = self.client.clone();
        let config = self.config.clone();
        let recorder = self.recorder.clone();
        Box::pin(try_stream! {
            let ModelInvocation { call_id, request, .. } = invocation;
            let mut messages = anthropic_messages(&request.history)?;
            mark_cache_breakpoint(&mut messages);
            let system = if request.prompt.instructions.is_empty() {
                Value::String(String::new())
            } else {
                json!([{
                    "type": "text",
                    "text": request.prompt.instructions,
                    "cache_control": {"type": "ephemeral"}
                }])
            };
            let max_tokens = request.model.max_output_tokens.or(config.max_output_tokens)
                .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
            let mut body = json!({
                "model": request.model.model_id, "system": system, "messages": messages,
                "max_tokens": max_tokens, "stream": true
            });
            if !request.prompt.tools.is_empty() {
                let tool_count = request.prompt.tools.len();
                body["tools"] = json!(request.prompt.tools.iter().enumerate().map(|(index, tool)| {
                    let mut value = json!({
                        "name": tool.name, "description": tool.description, "input_schema": tool.parameters
                    });
                    if index + 1 == tool_count {
                        value["cache_control"] = json!({"type": "ephemeral"});
                    }
                    value
                }).collect::<Vec<_>>());
            }
            apply_model(&mut body, &request.model)?;
            merge_extra_params(&mut body, &request.model.extra_params)?;
            let request_headers = recorded_headers(
                &config,
                &[("content-type", "application/json"), ("anthropic-version", "2023-06-01")],
            );
            if let Some(recorder) = &recorder {
                recorder.request(request_headers.clone(), &body).await?;
            }
            let attempt = send_once(
                "Anthropic",
                || client.post(&config.request_url)
                    .header("x-api-key", &config.api_key).header("anthropic-version", "2023-06-01")
                    .headers(config.custom_headers.clone())
                    .json(&body),
                &cancellation,
                recorder.as_ref(),
            ).await?;
            let Attempt::Response(response) = attempt else { return };
            yield ModelEvent::Start { model_call_id: call_id };
            let chunk_recorder = recorder.clone();
            let chunks = response.bytes_stream()
                .map(|chunk| chunk.map_err(Error::from))
                .then(move |chunk| {
                    let recorder = chunk_recorder.clone();
                    async move {
                        let chunk = chunk?;
                        if let Some(recorder) = recorder { recorder.response_chunk(&chunk).await?; }
                        Ok::<_, Error>(chunk)
                    }
                });
            let source = chunks.eventsource();
            futures_util::pin_mut!(source);
            let mut block_types = std::collections::BTreeMap::<usize, String>::new();
            let mut thinking_text = std::collections::HashMap::<usize, String>::new();
            let mut thinking_signatures = std::collections::HashMap::<usize, String>::new();
            let mut thinking_blocks = std::collections::BTreeMap::new();
            let mut finish = None;
            let mut saw_tool = false;
            let mut saw_message_stop = false;
            let mut final_usage = None::<Usage>;
            while let Some(event) = tokio::select! {
                biased;
                _ = cancellation.cancelled() => { return; }
                event = source.next() => event,
            } {
                let event = event.map_err(|error| map_sse_error("Anthropic", error))?;
                let value: Value = serde_json::from_str(&event.data)?;
                if let Some(error) = provider_event_error("Anthropic", &event.event, &value) {
                    Err(error)?;
                }
                let data_kind = value.get("type").and_then(Value::as_str);
                let kind = match event.event.as_str() {
                    "" | "message" => data_kind.unwrap_or(event.event.as_str()),
                    kind => kind,
                };
                match kind {
                    "message_start" => if let Some(usage) = value.pointer("/message/usage") {
                        merge_usage(final_usage.get_or_insert_default(), anthropic_usage(usage));
                    },
                    "content_block_start" => {
                        let index = required_u64(&value, "index")? as usize;
                        let block = value.get("content_block").unwrap_or(&Value::Null);
                        let kind = required_string(block, "type")?;
                        block_types.insert(index, kind.into());
                        match kind {
                            "text" => {
                                yield ModelEvent::TextStart;
                                if let Some(text) = block.get("text").and_then(Value::as_str).filter(|text| !text.is_empty()) {
                                    yield ModelEvent::TextDelta(text.into());
                                }
                            }
                            "thinking" => {
                                let text = block.get("thinking").and_then(Value::as_str).unwrap_or_default();
                                let signature = block.get("signature").and_then(Value::as_str).unwrap_or_default();
                                thinking_text.insert(index, text.into());
                                thinking_signatures.insert(index, signature.into());
                                yield ModelEvent::ThinkingStart;
                                if !text.is_empty() { yield ModelEvent::ThinkingDelta(text.into()); }
                            }
                            "redacted_thinking" => { thinking_blocks.insert(index, block.clone()); }
                            "tool_use" => {
                                saw_tool = true;
                                yield ModelEvent::ToolCallStart {
                                    index,
                                    call_id: required_string(block, "id")?.into(),
                                    name: required_string(block, "name")?.into(),
                                };
                                if let Some(input) = block.get("input").and_then(Value::as_object).filter(|input| !input.is_empty()) {
                                    yield ModelEvent::ToolCallArgumentsDelta { index, delta: serde_json::to_string(input)? };
                                }
                            }
                            _ => {}
                        }
                    }
                    "content_block_delta" => {
                        let index = required_u64(&value, "index")? as usize;
                        let delta = value.get("delta").unwrap_or(&Value::Null);
                        let delta_kind = required_string(delta, "type")?;
                        if let std::collections::btree_map::Entry::Vacant(entry) = block_types.entry(index) {
                            match delta_kind {
                                "text_delta" => {
                                    entry.insert("text".into());
                                    yield ModelEvent::TextStart;
                                }
                                "thinking_delta" | "signature_delta" => {
                                    entry.insert("thinking".into());
                                    thinking_text.insert(index, String::new());
                                    thinking_signatures.insert(index, String::new());
                                    yield ModelEvent::ThinkingStart;
                                }
                                _ => {}
                            }
                        }
                        match delta_kind {
                            "text_delta" => if let Some(text) = delta.get("text").and_then(Value::as_str) { yield ModelEvent::TextDelta(text.into()); },
                            "thinking_delta" => if let Some(text) = delta.get("thinking").and_then(Value::as_str) {
                                thinking_text.entry(index).or_default().push_str(text);
                                yield ModelEvent::ThinkingDelta(text.into());
                            },
                            "signature_delta" => if let Some(signature) = delta.get("signature").and_then(Value::as_str) {
                                thinking_signatures.entry(index).or_default().push_str(signature);
                            },
                            "input_json_delta" => if let Some(text) = delta.get("partial_json").and_then(Value::as_str) { yield ModelEvent::ToolCallArgumentsDelta { index, delta: text.into() }; },
                            _ => {}
                        }
                    }
                    "content_block_stop" => {
                        let index = required_u64(&value, "index")? as usize;
                        if let Some(kind) = block_types.remove(&index) {
                            for event in close_anthropic_block(index, &kind, &mut thinking_text, &mut thinking_signatures, &mut thinking_blocks) {
                                yield event;
                            }
                        }
                    }
                    "message_delta" => {
                        if let Some(usage) = value.get("usage") {
                            merge_usage(final_usage.get_or_insert_default(), anthropic_usage(usage));
                        }
                        finish = match value.pointer("/delta/stop_reason").and_then(Value::as_str) {
                            Some("tool_use") => Some(FinishReason::ToolUse),
                            Some("max_tokens" | "model_context_window_exceeded") => Some(FinishReason::Length),
                            Some("end_turn" | "stop_sequence" | "pause_turn" | "refusal") => Some(FinishReason::Stop),
                            None => finish,
                            Some(_) => Some(if saw_tool { FinishReason::ToolUse } else { FinishReason::Stop }),
                        };
                    }
                    "message_stop" => {
                        saw_message_stop = true;
                        break;
                    }
                    _ => {}
                }
            }
            let finish = finish.or_else(|| saw_message_stop.then_some(if saw_tool { FinishReason::ToolUse } else { FinishReason::Stop }))
                .ok_or_else(|| Error::provider("Anthropic stream ended without message_stop".into()))?;
            for (index, kind) in block_types {
                for event in close_anthropic_block(index, &kind, &mut thinking_text, &mut thinking_signatures, &mut thinking_blocks) {
                    yield event;
                }
            }
            if !thinking_blocks.is_empty() {
                yield ModelEvent::ProviderReplayState(crate::model::ProviderReplayState {
                    provider_kind: "anthropic".into(),
                    value: json!({"blocks": thinking_blocks.into_values().collect::<Vec<_>>()}),
                });
            }
            if let Some(usage) = final_usage { yield ModelEvent::Usage(usage); }
            yield ModelEvent::Done(finish);
        })
    }
}

fn close_anthropic_block(
    index: usize,
    kind: &str,
    thinking_text: &mut std::collections::HashMap<usize, String>,
    thinking_signatures: &mut std::collections::HashMap<usize, String>,
    thinking_blocks: &mut std::collections::BTreeMap<usize, Value>,
) -> Vec<ModelEvent> {
    match kind {
        "text" => vec![ModelEvent::TextEnd],
        "thinking" => {
            let thinking = thinking_text.remove(&index).unwrap_or_default();
            let signature = thinking_signatures.remove(&index).unwrap_or_default();
            if !signature.is_empty() {
                thinking_blocks.insert(
                    index,
                    json!({
                        "type": "thinking",
                        "thinking": thinking,
                        "signature": signature,
                    }),
                );
            }
            vec![ModelEvent::ThinkingEnd]
        }
        "tool_use" => vec![ModelEvent::ToolCallEnd { index }],
        _ => Vec::new(),
    }
}

pub(super) fn apply_saved_thinking_budget(
    model: &mut crate::model::ModelSpec,
    saved_budget: Option<u64>,
    config: &ProviderConfig,
) -> Result<()> {
    let Some(budget) = saved_budget else {
        return Ok(());
    };
    match model.extra_params.get("thinking") {
        Some(thinking)
            if thinking.get("type").and_then(Value::as_str) == Some("enabled")
                && thinking.get("budget_tokens").is_none() => {}
        None if matches!(
            model.model_id.as_str(),
            "claude-haiku-4-5"
                | "claude-haiku-4-5-20251001"
                | "claude-sonnet-4-5"
                | "claude-sonnet-4-5-20250929"
                | "claude-opus-4-5"
                | "claude-opus-4-5-20251101"
                | "claude-sonnet-4-6"
                | "claude-opus-4-6"
                | "claude-mythos-preview"
        ) => {}
        _ => return Ok(()),
    }
    let max_tokens = model
        .extra_params
        .get("max_tokens")
        .and_then(Value::as_u64)
        .or(model.max_output_tokens)
        .or(config.max_output_tokens)
        .unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
    let interleaved = config
        .custom_headers
        .get_all("anthropic-beta")
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .any(|value| value.trim() == "interleaved-thinking-2025-05-14");
    if budget < 1024 || (!interleaved && budget >= max_tokens) {
        return Err(Error::Config(
            "Anthropic thinking budget must be at least 1024 tokens and below max_tokens unless interleaved thinking is enabled".into(),
        ));
    }
    if let Some(thinking) = model.extra_params.get_mut("thinking") {
        thinking["budget_tokens"] = json!(budget);
    } else {
        model.extra_params["thinking"] = json!({"type":"enabled", "budget_tokens":budget});
    }
    Ok(())
}

fn apply_model(body: &mut Value, model: &crate::model::ModelSpec) -> Result<()> {
    let object = body
        .as_object_mut()
        .ok_or_else(|| Error::provider("Anthropic request body is not an object".into()))?;
    if model.reasoning.enabled {
        object.insert(
            "thinking".into(),
            json!({"type":"adaptive", "display":"summarized"}),
        );
    }
    if model
        .extra_params
        .pointer("/thinking/type")
        .and_then(Value::as_str)
        != Some("enabled")
    {
        if let Some(effort) = &model.reasoning.effort {
            object.insert("output_config".into(), json!({"effort":effort}));
        }
    }
    Ok(())
}

fn merge_usage(total: &mut Usage, update: Usage) {
    merge_usage_field(&mut total.input_tokens, update.input_tokens);
    merge_usage_field(&mut total.output_tokens, update.output_tokens);
    merge_usage_field(&mut total.total_tokens, update.total_tokens);
    merge_usage_field(&mut total.cache_read_tokens, update.cache_read_tokens);
    merge_usage_field(&mut total.cache_write_tokens, update.cache_write_tokens);
    merge_usage_field(&mut total.reasoning_tokens, update.reasoning_tokens);
    total.context_input_tokens = anthropic_context_input(total);
    if let Some(request_tokens) = total
        .context_input_tokens
        .zip(total.output_tokens)
        .and_then(|(input, output)| input.checked_add(output))
    {
        total.total_tokens = Some(request_tokens);
    }
}

fn merge_usage_field(total: &mut Option<u64>, update: Option<u64>) {
    if let Some(update) = update {
        *total = Some(total.map_or(update, |current| current.max(update)));
    }
}

fn anthropic_messages(messages: &[ProjectedMessage]) -> Result<Vec<Value>> {
    let mut output = Vec::new();
    for message in messages {
        match &message.content {
            ProjectedContent::Parts(parts) => {
                let content = anthropic_parts(&message.role, parts)?;
                if !content.is_empty() {
                    push_anthropic(&mut output, role_name(&message.role), content);
                }
            }
            ProjectedContent::ToolResult(result) => {
                let content = if result.provider_parts.is_empty() {
                    Value::String(result.content.clone())
                } else {
                    Value::Array(anthropic_parts(&Role::User, &result.provider_parts)?)
                };
                let mut block = json!({
                    "type": "tool_result",
                    "tool_use_id": result.call_id,
                    "content": content,
                });
                if result.is_error {
                    block["is_error"] = json!(true);
                }
                push_anthropic(&mut output, "user", vec![block]);
            }
            ProjectedContent::Assistant {
                text,
                replay_state,
                calls,
                ..
            } => {
                let mut content = Vec::new();
                if let Some(blocks) = replay_state
                    .as_ref()
                    .filter(|state| state.provider_kind == "anthropic")
                    .and_then(|state| state.value.get("blocks"))
                    .and_then(Value::as_array)
                {
                    content.extend(blocks.iter().cloned());
                }
                if !text.is_empty() {
                    content.push(json!({"type": "text", "text": text}));
                }
                content.extend(calls.iter().map(|call| {
                    json!({
                        "type": "tool_use",
                        "id": call.call_id,
                        "name": call.name,
                        "input": call.arguments,
                    })
                }));
                if !content.is_empty() {
                    push_anthropic(&mut output, "assistant", content);
                }
            }
        }
    }
    Ok(output)
}

fn mark_cache_breakpoint(messages: &mut [Value]) {
    let Some(message) = messages
        .iter_mut()
        .rev()
        .find(|message| message.get("role").and_then(Value::as_str) == Some("user"))
    else {
        return;
    };
    let Some(content) = message.get_mut("content").and_then(Value::as_array_mut) else {
        return;
    };
    for block in content.iter_mut().rev() {
        let kind = block.get("type").and_then(Value::as_str);
        if !matches!(kind, Some("text" | "image" | "tool_result")) {
            continue;
        }
        if let Some(block) = block.as_object_mut() {
            block.insert("cache_control".into(), json!({"type": "ephemeral"}));
            return;
        }
    }
}

fn anthropic_parts(role: &Role, parts: &[ContentPart]) -> Result<Vec<Value>> {
    parts
        .iter()
        .filter_map(|part| match part {
            ContentPart::Text { text } if text.is_empty() => None,
            ContentPart::Text { text } => Some(Ok(json!({"type":"text", "text":text}))),
            ContentPart::Image { mime_type, data } if *role == Role::User => Some(Ok(json!({
                "type":"image",
                "source":{
                    "type":"base64",
                    "media_type":mime_type,
                    "data":STANDARD.encode(data),
                },
            }))),
            ContentPart::Image { .. } => Some(Err(Error::Protocol(
                "Anthropic only accepts images in user messages".into(),
            ))),
        })
        .collect()
}

fn push_anthropic(output: &mut Vec<Value>, role: &str, mut content: Vec<Value>) {
    if let Some(last) = output
        .last_mut()
        .filter(|last| last.get("role").and_then(Value::as_str) == Some(role))
    {
        if let Some(existing) = last.get_mut("content").and_then(Value::as_array_mut) {
            if content.first().is_some_and(is_tool_result) {
                let first_non_result = existing.partition_point(is_tool_result);
                existing.splice(first_non_result..first_non_result, content);
            } else {
                existing.append(&mut content);
            }
            return;
        }
    }
    output.push(json!({"role":role, "content":content}));
}

fn is_tool_result(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("tool_result")
}

fn role_name(role: &Role) -> &'static str {
    match role {
        Role::System => "user",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "user",
    }
}

fn required_string<'a>(value: &'a Value, name: &str) -> Result<&'a str> {
    value
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| Error::provider(format!("Anthropic event is missing {name}")))
}

fn required_u64(value: &Value, name: &str) -> Result<u64> {
    value
        .get(name)
        .and_then(Value::as_u64)
        .ok_or_else(|| Error::provider(format!("Anthropic event is missing {name}")))
}

pub(crate) fn anthropic_usage(value: &Value) -> Usage {
    let input_tokens = value.get("input_tokens").and_then(Value::as_u64);
    let cache_read_tokens = value.get("cache_read_input_tokens").and_then(Value::as_u64);
    let cache_write_tokens = value
        .get("cache_creation_input_tokens")
        .and_then(Value::as_u64);
    let mut usage = Usage {
        input_tokens,
        context_input_tokens: None,
        output_tokens: value.get("output_tokens").and_then(Value::as_u64),
        total_tokens: value.get("total_tokens").and_then(Value::as_u64),
        cache_read_tokens,
        cache_write_tokens,
        reasoning_tokens: value
            .pointer("/output_tokens_details/thinking_tokens")
            .and_then(Value::as_u64),
    };
    usage.context_input_tokens = anthropic_context_input(&usage);
    usage
}

fn anthropic_context_input(usage: &Usage) -> Option<u64> {
    usage.input_tokens.map(|input| {
        input
            .saturating_add(usage.cache_read_tokens.unwrap_or_default())
            .saturating_add(usage.cache_write_tokens.unwrap_or_default())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::ProviderKind,
        model::{ModelSpec, ToolCallContent, ToolResultContent},
        provider::tests::{collect_sse, invocation, sse_events},
    };

    #[test]
    fn saved_thinking_budget_is_used_only_for_fixed_mode() {
        let config = ProviderConfig {
            kind: ProviderKind::Anthropic,
            request_url: String::new(),
            api_key: String::new(),
            custom_headers: reqwest::header::HeaderMap::new(),
            max_output_tokens: Some(4096),
            request_timeout: std::time::Duration::from_secs(5),
            allowed_body_fields: None,
        };
        for (model_id, extra, expected, expected_effort) in [
            (
                "claude-haiku-4-5-20251001",
                json!({}),
                json!({"type":"enabled", "budget_tokens":2048}),
                Value::Null,
            ),
            (
                "claude-opus-4-7",
                json!({}),
                json!({"type":"adaptive", "display":"summarized"}),
                json!("high"),
            ),
            (
                "gateway/claude-haiku-4-5",
                json!({}),
                json!({"type":"adaptive", "display":"summarized"}),
                json!("high"),
            ),
            (
                "gateway",
                json!({"thinking":{"type":"enabled", "display":"omitted"}}),
                json!({"type":"enabled", "budget_tokens":2048, "display":"omitted"}),
                Value::Null,
            ),
            (
                "gateway",
                json!({"thinking":{"type":"enabled", "budget_tokens":3072}, "output_config":{"effort":"low"}}),
                json!({"type":"enabled", "budget_tokens":3072}),
                json!("low"),
            ),
            (
                "claude-sonnet-4-6",
                json!({"thinking":{"type":"adaptive"}}),
                json!({"type":"adaptive"}),
                json!("high"),
            ),
        ] {
            let mut model = ModelSpec::new(model_id);
            model.reasoning.enabled = true;
            model.reasoning.effort = Some("high".into());
            model.extra_params = extra;
            apply_saved_thinking_budget(&mut model, Some(2048), &config).unwrap();
            let mut body = json!({});
            apply_model(&mut body, &model).unwrap();
            merge_extra_params(&mut body, &model.extra_params).unwrap();
            assert_eq!(body["thinking"], expected, "{model_id}");
            assert_eq!(
                body["output_config"]["effort"], expected_effort,
                "{model_id}"
            );
        }
        let mut model = ModelSpec::new("claude-sonnet-4-6");
        assert!(apply_saved_thinking_budget(&mut model, Some(1023), &config).is_err());
        assert!(apply_saved_thinking_budget(&mut model, Some(4096), &config).is_err());
    }

    #[tokio::test]
    async fn message_stop_yields_once_and_signed_thinking_and_tool_results_replay() {
        let body = sse_events(&[
            (
                "message_start",
                json!({"message":{"usage":{"input_tokens":10,"output_tokens":0}}}),
            ),
            (
                "content_block_start",
                json!({"index":0,"content_block":{"type":"thinking","thinking":"initial ","signature":""}}),
            ),
            (
                "content_block_delta",
                json!({"index":0,"delta":{"type":"thinking_delta","thinking":"thought"}}),
            ),
            (
                "content_block_delta",
                json!({"index":0,"delta":{"type":"signature_delta","signature":"opaque"}}),
            ),
            ("content_block_stop", json!({"index":0})),
            (
                "content_block_start",
                json!({"index":1,"content_block":{"type":"redacted_thinking","data":"encrypted"}}),
            ),
            ("content_block_stop", json!({"index":1})),
            (
                "message_delta",
                json!({"delta":{"stop_reason":"end_turn"},"usage":{"cache_read_input_tokens":20,"output_tokens":7,"output_tokens_details":{"thinking_tokens":3}}}),
            ),
            ("message_stop", json!({})),
            ("message_stop", json!({})),
        ]);
        let (events, _) = collect_sse(
            ProviderKind::Anthropic,
            body,
            invocation(ModelSpec::new("test")),
        )
        .await;
        let events = events.into_iter().collect::<Result<Vec<_>>>().unwrap();
        let blocks = json!([
            {"type":"thinking","thinking":"initial thought","signature":"opaque"},
            {"type":"redacted_thinking","data":"encrypted"}
        ]);
        let replay = crate::model::ProviderReplayState {
            provider_kind: "anthropic".into(),
            value: json!({"blocks":blocks}),
        };
        assert_eq!(
            events,
            [
                ModelEvent::Start {
                    model_call_id: "call-1".into()
                },
                ModelEvent::ThinkingStart,
                ModelEvent::ThinkingDelta("initial ".into()),
                ModelEvent::ThinkingDelta("thought".into()),
                ModelEvent::ThinkingEnd,
                ModelEvent::ProviderReplayState(replay.clone()),
                ModelEvent::Usage(Usage {
                    input_tokens: Some(10),
                    context_input_tokens: Some(30),
                    output_tokens: Some(7),
                    total_tokens: Some(37),
                    cache_read_tokens: Some(20),
                    reasoning_tokens: Some(3),
                    ..Usage::default()
                }),
                ModelEvent::Done(FinishReason::Stop),
            ]
        );

        let mut invocation = invocation(ModelSpec::new("test"));
        invocation.request.history = vec![
            ProjectedMessage {
                message_id: "assistant".into(),
                role: Role::Assistant,
                content: ProjectedContent::Assistant {
                    text: String::new(),
                    thinking: String::new(),
                    replay_state: Some(replay),
                    calls: ["tool-a", "tool-b"]
                        .into_iter()
                        .enumerate()
                        .map(|(index, call_id)| ToolCallContent {
                            index,
                            call_id: call_id.into(),
                            name: "lookup".into(),
                            arguments: json!({}),
                        })
                        .collect(),
                },
            },
            ProjectedMessage {
                message_id: "follow-up".into(),
                role: Role::User,
                content: ProjectedContent::Parts(vec![ContentPart::Text {
                    text: "Continue".into(),
                }]),
            },
        ];
        invocation
            .request
            .history
            .extend([("tool-a", false), ("tool-b", true)].into_iter().map(
                |(call_id, is_error)| ProjectedMessage {
                    message_id: call_id.into(),
                    role: Role::Tool,
                    content: ProjectedContent::ToolResult(ToolResultContent {
                        call_id: call_id.into(),
                        name: "lookup".into(),
                        content: "result".into(),
                        is_error,
                        image: None,
                        provider_parts: Vec::new(),
                    }),
                },
            ));
        let body = sse_events(&[("message_stop", json!({}))]);
        let (events, request) = collect_sse(ProviderKind::Anthropic, body, invocation).await;
        assert!(events.into_iter().all(|event| event.is_ok()));
        let assistant = request["messages"][0]["content"].as_array().unwrap();
        assert_eq!(&assistant[..2], blocks.as_array().unwrap());
        let user = &request["messages"][1]["content"];
        assert_eq!(
            user[0],
            json!({"type":"tool_result","tool_use_id":"tool-a","content":"result"})
        );
        assert_eq!(
            user[1],
            json!({"type":"tool_result","tool_use_id":"tool-b","content":"result","is_error":true})
        );
        assert_eq!(user[2]["text"], "Continue");
    }

    #[test]
    fn cached_tokens_are_included_once_in_anthropic_context_input() {
        let usage = anthropic_usage(&serde_json::json!({
            "input_tokens": 10,
            "output_tokens": 5,
            "cache_read_input_tokens": 20,
            "cache_creation_input_tokens": 30
        }));

        assert_eq!(usage.input_tokens, Some(10));
        assert_eq!(usage.context_input_tokens, Some(60));
        assert_eq!(usage.output_tokens, Some(5));
    }

    #[test]
    fn streamed_anthropic_usage_includes_cached_input_in_total() {
        let mut usage = Usage::default();
        merge_usage(
            &mut usage,
            anthropic_usage(&serde_json::json!({
                "input_tokens": 414,
                "output_tokens": 0,
                "cache_read_input_tokens": 100_352,
                "cache_creation_input_tokens": 0
            })),
        );
        merge_usage(
            &mut usage,
            anthropic_usage(&serde_json::json!({"output_tokens": 191})),
        );

        assert_eq!(usage.input_tokens, Some(414));
        assert_eq!(usage.cache_read_tokens, Some(100_352));
        assert_eq!(usage.cache_write_tokens, Some(0));
        assert_eq!(usage.context_input_tokens, Some(100_766));
        assert_eq!(usage.output_tokens, Some(191));
        assert_eq!(usage.total_tokens, Some(100_957));
    }
}
