//! OpenAI-compatible streaming provider, ported from `src/model/provider.ts`.

use crate::types::{Message, ReasoningLevel, ToolCall, ToolDeclaration};
use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use futures_util::stream::BoxStream;
use futures_util::StreamExt;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ProviderKind {
    #[serde(rename = "openrouter")]
    OpenRouter,
    #[serde(rename = "opencode-go")]
    OpenCodeGo,
    #[serde(rename = "anthropic")]
    Anthropic,
    #[serde(rename = "custom")]
    Custom,
}

impl ProviderKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::OpenRouter => "openrouter",
            Self::OpenCodeGo => "opencode-go",
            Self::Anthropic => "anthropic",
            Self::Custom => "custom",
        }
    }

    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "openrouter" => Some(Self::OpenRouter),
            "opencode-go" => Some(Self::OpenCodeGo),
            "anthropic" => Some(Self::Anthropic),
            "custom" => Some(Self::Custom),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
pub struct ProviderConfig {
    pub id: ProviderKind,
    pub name: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub session_id: Option<String>,
    /// Ordered extra headers; kept ordered for deterministic requests.
    pub headers: Vec<(String, String)>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub retry_delay_ms: Option<u64>,
    pub context_window: Option<u64>,
}

#[derive(Clone, Debug, Default)]
pub struct ProviderRequest {
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDeclaration>,
    /// Optional per-request model override, used by compaction/summarization.
    pub model: Option<String>,
    /// Reasoning amount for providers that support it.
    pub reasoning: Option<ReasoningLevel>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum ProviderEvent {
    TextDelta { text: String },
    ToolCallDelta { index: usize, id: Option<String>, name: Option<String>, arguments_delta: Option<String> },
    Usage { prompt_tokens: u64, completion_tokens: u64, cached_tokens: u64 },
    Done { finish_reason: String },
}

#[async_trait]
pub trait Provider: Send + Sync {
    fn config(&self) -> &ProviderConfig;

    fn stream<'a>(&'a self, request: ProviderRequest) -> BoxStream<'a, Result<ProviderEvent>>;
}

pub fn chat_url(base_url: &str) -> String {
    if base_url.ends_with("/chat/completions") {
        base_url.to_string()
    } else {
        format!("{}/chat/completions", base_url.trim_end_matches('/'))
    }
}

pub fn openai_messages(messages: &[Message]) -> Value {
    Value::Array(
        messages
            .iter()
            .map(|message| match message.role {
                crate::types::Role::User if !message.images.is_empty() => json!({
                    "role": "user",
                    "content": [
                        {"type": "text", "text": message.content},
                        ..message.images.iter().map(|image| json!({"type": "image_url", "image_url": {"url": format!("data:{};base64,{}", image.mime_type, image.data)}})).collect::<Vec<_>>()
                    ]
                }),
                crate::types::Role::Tool => json!({
                    "role": "tool",
                    "content": message.content,
                    "tool_call_id": message.tool_call_id.clone().unwrap_or_default(),
                    "name": message.tool_name.clone(),
                }),
                crate::types::Role::Assistant if !message.tool_calls.is_empty() => json!({
                    "role": "assistant",
                    "content": if message.content.is_empty() { Value::Null } else { Value::String(message.content.clone()) },
                    "tool_calls": message.tool_calls.iter().map(|call| json!({
                        "id": call.id, "type": "function",
                        "function": {"name": call.name, "arguments": serde_json::to_string(&call.arguments).unwrap_or_default()}
                    })).collect::<Vec<_>>(),
                }),
                _ => json!({"role": message.role, "content": message.content}),
            })
            .collect(),
    )
}

/// Parse one SSE `data:` payload; `[DONE]` produces no events.
pub fn parse_sse_data(data: &str) -> Vec<Value> {
    if data.is_empty() || data == "[DONE]" {
        return Vec::new();
    }
    serde_json::from_str::<Value>(data).map(|value| vec![value]).unwrap_or_default()
}

fn retryable(status: u16) -> bool {
    status == 408 || status == 425 || status == 429 || status >= 500
}

pub struct OpenAICompatibleProvider {
    pub config: ProviderConfig,
    client: reqwest::Client,
}

impl OpenAICompatibleProvider {
    pub fn new(config: ProviderConfig) -> Self {
        let mut config = config;
        config.session_id.get_or_insert_with(|| uuid::Uuid::new_v4().to_string());
        let mut builder = reqwest::Client::builder();
        if let Some(timeout_ms) = config.timeout_ms {
            builder = builder.timeout(Duration::from_millis(timeout_ms));
        }
        let client = builder.build().unwrap_or_default();
        Self { config, client }
    }

    fn headers(&self) -> reqwest::header::HeaderMap {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("content-type", "application/json".parse().unwrap());
        headers.insert("accept", "text/event-stream".parse().unwrap());
        if let Some(api_key) = &self.config.api_key {
            if let Ok(value) = reqwest::header::HeaderValue::from_str(&format!("Bearer {api_key}")) {
                headers.insert("authorization", value);
            }
        }
        if self.config.id == ProviderKind::OpenCodeGo {
            if let Some(session_id) = &self.config.session_id {
                if let Ok(value) = reqwest::header::HeaderValue::from_str(session_id) {
                    headers.insert("x-opencode-session", value);
                }
            }
            headers.insert("x-opencode-client", "hl-harness".parse().unwrap());
        }
        if self.config.id == ProviderKind::OpenRouter {
            headers.insert("HTTP-Referer", "https://github.com/".parse().unwrap());
            headers.insert("X-Title", "hl-harness".parse().unwrap());
        }
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

    fn body(&self, request: &ProviderRequest) -> String {
        let mut body = json!({
            "model": request.model.clone().unwrap_or_else(|| self.config.model.clone()),
            "messages": openai_messages(&request.messages),
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        // OpenAI-style reasoning effort (OpenRouter/o-series/compatible gateways).
        if let Some(reasoning) = request.reasoning.filter(|level| *level != ReasoningLevel::Off) {
            body["reasoning_effort"] = json!(reasoning.as_str());
        }
        if !request.tools.is_empty() {
            body["tools"] = json!(request
                .tools
                .iter()
                .map(|tool| json!({"type": "function", "function": tool}))
                .collect::<Vec<_>>());
        }
        serde_json::to_string(&body).unwrap_or_default()
    }

    async fn fetch_response(&self, request: &ProviderRequest) -> Result<reqwest::Response> {
        let max_retries = self.config.max_retries.unwrap_or(2);
        let delay = self.config.retry_delay_ms.unwrap_or(250);
        let mut attempt = 0;
        loop {
            let result = self
                .client
                .post(chat_url(&self.config.base_url))
                .headers(self.headers())
                .body(self.body(request))
                .send()
                .await;
            match result {
                Ok(response) => {
                    let status = response.status().as_u16();
                    if !retryable(status) || attempt >= max_retries {
                        return Ok(response);
                    }
                    let _ = response.text().await;
                }
                Err(error) => {
                    if attempt >= max_retries {
                        return Err(error.into());
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(delay * 2u64.pow(attempt))).await;
            attempt += 1;
        }
    }
}

/// Consume one SSE block into provider events; `Err` propagates stream errors.
fn consume_block(block: &str, next_tool_index: &mut usize, finish_reason: &mut String) -> Result<Vec<ProviderEvent>> {
    let mut events = Vec::new();
    for line in block.split('\n') {
        let line = line.trim_end_matches('\r');
        if !line.starts_with("data:") {
            continue;
        }
        for raw in parse_sse_data(line[5..].trim()) {
            if raw.get("error").is_some() {
                let error = &raw["error"];
                let message = if error.is_string() {
                    error.as_str().unwrap_or("Provider stream error").to_string()
                } else {
                    error["message"].as_str().unwrap_or("Provider stream error").to_string()
                };
                bail!(message);
            }
            let choice = &raw["choices"][0];
            if let Some(content) = choice["delta"]["content"].as_str() {
                if !content.is_empty() {
                    events.push(ProviderEvent::TextDelta { text: content.to_string() });
                }
            }
            if let Some(calls) = choice["delta"]["tool_calls"].as_array() {
                for call in calls {
                    let index = match call["index"].as_u64() {
                        Some(index) => index as usize,
                        None => {
                            let index = *next_tool_index;
                            *next_tool_index += 1;
                            index
                        }
                    };
                    events.push(ProviderEvent::ToolCallDelta {
                        index,
                        id: call["id"].as_str().map(str::to_string),
                        name: call["function"]["name"].as_str().map(str::to_string),
                        arguments_delta: call["function"]["arguments"].as_str().map(str::to_string),
                    });
                }
            }
            if let Some(reason) = choice["finish_reason"].as_str() {
                *finish_reason = reason.to_string();
            }
            if let Some(usage) = raw.get("usage").filter(|value| !value.is_null()) {
                events.push(ProviderEvent::Usage {
                    prompt_tokens: usage["prompt_tokens"].as_u64().unwrap_or(0),
                    completion_tokens: usage["completion_tokens"].as_u64().unwrap_or(0),
                    cached_tokens: usage["prompt_tokens_details"]["cached_tokens"].as_u64().unwrap_or(0),
                });
            }
        }
    }
    Ok(events)
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
impl Provider for OpenAICompatibleProvider {
    fn config(&self) -> &ProviderConfig {
        &self.config
    }

    fn stream(&self, request: ProviderRequest) -> BoxStream<'_, Result<ProviderEvent>> {
        let this = self;
        Box::pin(async_stream::try_stream! {
            let response = this.fetch_response(&request).await?;
            let status = response.status();
            if !status.is_success() {
                let body = response.text().await.unwrap_or_default().trim().to_string();
                let mut message = format!("{} request failed ({status})", this.config.name);
                if let Ok(parsed) = serde_json::from_str::<Value>(&body) {
                    let error = &parsed["error"];
                    if !error.is_null() {
                        message = format!("{message}: {}", if error.is_string() { error.as_str().unwrap_or_default() } else { error["message"].as_str().unwrap_or(&body) });
                    }
                } else if !body.is_empty() {
                    message = format!("{message}: {body}");
                }
                Err(anyhow::Error::msg(message))?;
            } else {
                let mut buffer = String::new();
                let mut next_tool_index = 0usize;
                let mut finish_reason = "stop".to_string();
                let mut stream = response.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    buffer.push_str(&String::from_utf8_lossy(&chunk?));
                    while let Some(block) = split_blocks(&mut buffer) {
                        for event in consume_block(&block, &mut next_tool_index, &mut finish_reason)? {
                            yield event;
                        }
                    }
                }
                if !buffer.is_empty() {
                    for event in consume_block(&buffer, &mut next_tool_index, &mut finish_reason)? {
                        yield event;
                    }
                }
                yield ProviderEvent::Done { finish_reason };
            }
        })
    }
}

pub type SharedProvider = Arc<dyn Provider>;

/// Stable identity for a tool call, mirroring the TS `toolCallKey()`.
pub fn tool_call_key(call: &crate::types::ToolCall) -> String {
    format!("{}:{}", call.name, serde_json::to_string(&call.arguments).unwrap_or_default())
}
