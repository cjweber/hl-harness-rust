//! Native Anthropic Messages streaming provider, ported from `src/model/anthropic.ts`.

use super::provider::{Provider, ProviderConfig, ProviderEvent, ProviderRequest};
use crate::types::{Message, Role, ToolDeclaration};
use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use futures_util::stream::BoxStream;
use futures_util::StreamExt;
use serde_json::{json, Map, Value};
use std::time::Duration;

fn anthropic_messages(messages: &[Message]) -> (String, Value) {
    let system = messages
        .iter()
        .filter(|message| message.role == Role::System)
        .map(|message| message.content.clone())
        .collect::<Vec<_>>()
        .join("\n\n");
    let body = Value::Array(
        messages
            .iter()
            .filter(|message| message.role != Role::System)
            .map(|message| match message.role {
                Role::User if !message.images.is_empty() => json!({
                    "role": "user",
                    "content": [
                        {"type": "text", "text": message.content},
                        ..message.images.iter().map(|image| json!({"type": "image", "source": {"type": "base64", "media_type": image.mime_type, "data": image.data}})).collect::<Vec<_>>()
                    ]
                }),
                Role::Tool => json!({
                    "role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": message.tool_call_id.clone().unwrap_or_default(), "content": message.content, "tool_name": message.tool_name.clone()}],
                }),
                Role::Assistant if !message.tool_calls.is_empty() => json!({
                    "role": "assistant",
                    "content": [
                        {"type": "text", "text": message.content},
                        ..message.tool_calls.iter().map(|call| json!({"type": "tool_use", "id": call.id, "name": call.name, "input": call.arguments})).collect::<Vec<_>>()
                    ]
                }),
                _ => json!({"role": message.role, "content": message.content}),
            })
            .collect(),
    );
    (system, body)
}

fn tool_declaration(input: &ToolDeclaration) -> Value {
    json!({"name": input.name, "description": input.description, "input_schema": input.parameters})
}

fn retryable(status: u16) -> bool {
    status == 408 || status == 429 || status >= 500
}

#[derive(Default)]
struct StreamState {
    blocks: Vec<(usize, String, String, String)>, // (index, id, name, arguments)
    prompt_tokens: u64,
    completion_tokens: u64,
    cached_tokens: u64,
    finish_reason: String,
}

fn consume_event(block: &str, state: &mut StreamState) -> Result<Vec<ProviderEvent>> {
    let mut event = String::new();
    let mut data = String::new();
    for line in block.split('\n') {
        let line = line.trim_end_matches('\r');
        if let Some(value) = line.strip_prefix("event:") {
            event = value.trim().to_string();
        }
        if let Some(value) = line.strip_prefix("data:") {
            data.push_str(value.trim());
        }
    }
    if data.is_empty() {
        return Ok(Vec::new());
    }
    let value = serde_json::from_str::<Value>(&data)?;
    let mut events = Vec::new();
    match event.as_str() {
        "message_start" => {
            state.prompt_tokens = value["message"]["usage"]["input_tokens"].as_u64().unwrap_or(0);
            state.cached_tokens = value["message"]["usage"]["cache_read_input_tokens"].as_u64().unwrap_or(0);
        }
        "content_block_start" if value["content_block"]["type"] == "tool_use" => {
            let index = value["index"].as_u64().unwrap_or(0) as usize;
            state.blocks.push((
                index,
                value["content_block"]["id"].as_str().unwrap_or_default().to_string(),
                value["content_block"]["name"].as_str().unwrap_or_default().to_string(),
                String::new(),
            ));
        }
        "content_block_delta" => {
            let delta = &value["delta"];
            if delta["type"] == "text_delta" {
                if let Some(text) = delta["text"].as_str() {
                    events.push(ProviderEvent::TextDelta { text: text.to_string() });
                }
            } else if delta["type"] == "input_json_delta" {
                let index = value["index"].as_u64().unwrap_or(0) as usize;
                if let Some(slot) = state.blocks.iter_mut().find(|(block_index, ..)| *block_index == index) {
                    slot.3.push_str(delta["partial_json"].as_str().unwrap_or_default());
                }
            }
        }
        "message_delta" => {
            if let Some(reason) = value["delta"]["stop_reason"].as_str() {
                state.finish_reason = reason.to_string();
            }
            if let Some(tokens) = value["usage"]["output_tokens"].as_u64() {
                state.completion_tokens = tokens;
            }
        }
        "error" => bail!(value["error"]["message"].as_str().unwrap_or("Anthropic stream error").to_string()),
        _ => {}
    }
    Ok(events)
}

pub struct AnthropicProvider {
    pub config: ProviderConfig,
    client: reqwest::Client,
}

impl AnthropicProvider {
    pub fn new(config: ProviderConfig) -> Self {
        let mut builder = reqwest::Client::builder();
        if let Some(timeout_ms) = config.timeout_ms {
            builder = builder.timeout(Duration::from_millis(timeout_ms));
        }
        let client = builder.build().unwrap_or_default();
        Self { config, client }
    }

    fn body(&self, request: &ProviderRequest) -> String {
        let (system, messages) = anthropic_messages(&request.messages);
        let mut body = Map::new();
        body.insert("model".into(), json!(request.model.clone().unwrap_or_else(|| self.config.model.clone())));
        body.insert("max_tokens".into(), json!(4096));
        if !system.is_empty() {
            body.insert("system".into(), json!(system));
        }
        body.insert("messages".into(), messages);
        if !request.tools.is_empty() {
            body.insert("tools".into(), json!(request.tools.iter().map(tool_declaration).collect::<Vec<_>>()));
        }
        body.insert("stream".into(), json!(true));
        serde_json::to_string(&Value::Object(body)).unwrap_or_default()
    }

    fn headers(&self) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert("accept", "text/event-stream".parse().unwrap());
        headers.insert("x-api-key", self.config.api_key.clone().unwrap_or_default().parse().unwrap());
        headers.insert("anthropic-version", "2023-06-01".parse().unwrap());
        for (name, value) in &self.config.headers {
            if let (Ok(name), Ok(value)) = (
                reqwest::header::HeaderName::from_bytes(name.as_bytes()),
                reqwest::header::HeaderValue::from_str(value),
            ) {
                headers.insert(name, value);
            }
        }
        headers
    }
}

fn split_blocks(buffer: &mut String) -> Option<String> {
    let index = buffer.find("\n\n")?;
    let (block, rest) = if index > 0 && buffer.as_bytes()[index - 1] == b'\r' {
        (buffer[..index - 1].to_string(), buffer[index + 2..].to_string())
    } else {
        (buffer[..index].to_string(), buffer[index + 2..].to_string())
    };
    *buffer = rest;
    Some(block)
}

#[async_trait]
impl Provider for AnthropicProvider {
    fn config(&self) -> &ProviderConfig {
        &self.config
    }

    fn stream(&self, request: ProviderRequest) -> BoxStream<'_, Result<ProviderEvent>> {
        let this = self;
        Box::pin(async_stream::try_stream! {
            let url = format!("{}/messages", this.config.base_url.trim_end_matches('/'));
            let max_retries = this.config.max_retries.unwrap_or(2);
            let delay = this.config.retry_delay_ms.unwrap_or(250);
            let mut response: Option<reqwest::Response> = None;
            for attempt in 0..=max_retries {
                let result = this.client.post(&url).headers(this.headers()).body(this.body(&request)).send().await;
                match result {
                    Ok(candidate) => {
                        let status = candidate.status().as_u16();
                        if !retryable(status) || attempt == max_retries {
                            response = Some(candidate);
                            break;
                        }
                        let _ = candidate.text().await;
                    }
                    Err(error) => {
                        if attempt == max_retries {
                            Err::<(), _>(error)?;
                        }
                    }
                }
                tokio::time::sleep(Duration::from_millis(delay * 2u64.pow(attempt))).await;
            }
            let response = response.ok_or_else(|| anyhow!("{} returned no response", this.config.name))?;
            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default().trim().to_string();
                Err::<(), _>(anyhow!("{} request failed ({status}){}", this.config.name, if body.is_empty() { String::new() } else { format!(": {body}") }))?;
            } else {
                let mut state = StreamState { finish_reason: "stop".to_string(), ..Default::default() };
                let mut buffer = String::new();
                let mut stream = response.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    buffer.push_str(&String::from_utf8_lossy(&chunk?));
                    while let Some(block) = split_blocks(&mut buffer) {
                        for event in consume_event(&block, &mut state)? {
                            yield event;
                        }
                    }
                }
                if !buffer.is_empty() {
                    for event in consume_event(&buffer, &mut state)? {
                        yield event;
                    }
                }
                state.blocks.sort_by_key(|(index, ..)| *index);
                for (index, id, name, arguments) in &state.blocks {
                    yield ProviderEvent::ToolCallDelta {
                        index: *index,
                        id: Some(id.clone()),
                        name: Some(name.clone()),
                        arguments_delta: Some(if arguments.is_empty() { "{}".to_string() } else { arguments.clone() }),
                    };
                }
                yield ProviderEvent::Usage { prompt_tokens: state.prompt_tokens, completion_tokens: state.completion_tokens, cached_tokens: state.cached_tokens };
                yield ProviderEvent::Done { finish_reason: state.finish_reason };
            }
        })
    }
}
