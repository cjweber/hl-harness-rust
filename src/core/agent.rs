//! Multi-step tool loop, ported from `src/core/agent.ts`.

use crate::cache::prefix::{create_cache_plan, CachePrefix};
use crate::model::provider::{Provider, ProviderEvent, ProviderRequest, ProviderKind, SharedProvider};
use crate::security::permissions::{operation_for, ApprovalResult, OperationRequest};
use crate::tools::Tool;
use crate::types::{HarnessEvent, ImageAttachment, Message, ReasoningLevel, Role, ToolCall};
use anyhow::{anyhow, bail, Result};
use std::future::Future;
use futures_util::StreamExt;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Instant;

pub type ApproveFn = dyn Fn(OperationRequest) -> Pin<Box<dyn Future<Output = ApprovalResult> + Send>> + Send + Sync;

pub struct CompactionOptions {
    pub max_messages: Option<usize>,
    pub keep_messages: Option<usize>,
    pub context_window: Option<u64>,
    pub summary_prompt: Option<String>,
    pub summary_model: Option<String>,
    pub summary_max_chars: Option<usize>,
    pub on_compaction: Option<Box<dyn Fn(&str, usize) + Send + Sync>>,
}

impl Default for CompactionOptions {
    fn default() -> Self {
        Self { max_messages: None, keep_messages: None, context_window: None, summary_prompt: None, summary_model: None, summary_max_chars: None, on_compaction: None }
    }
}

#[derive(Clone, Debug, Default)]
pub struct CompactionMetrics {
    pub estimated_tokens_before: usize,
    pub summary_chars: usize,
    pub summary_source: &'static str,
    pub summary_model: Option<String>,
}

pub struct AgentRunResult {
    pub text: String,
    pub steps: usize,
}

struct PendingCall {
    id: String,
    name: String,
    arguments: String,
}

pub struct Agent {
    provider: SharedProvider,
    observer: Option<Arc<dyn crate::extensions::AgentObserver>>,
    reasoning: ReasoningLevel,
    tools: Vec<Arc<dyn Tool>>,
    compaction: CompactionOptions,
    system_prompt: String,
    max_steps: usize,
    approve_operation: Option<Box<ApproveFn>>,
    messages: Vec<Message>,
    pub last_compaction_metrics: Option<CompactionMetrics>,
}

const DEFAULT_SUMMARY_PROMPT: &str = "Summarize the conversation for another coding agent. Preserve goals, decisions, changed files, commands, test results, unresolved problems, and next steps. Be concise and factual. Output only the summary.";

impl Agent {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        provider: SharedProvider,
        tools: Vec<Arc<dyn Tool>>,
        compaction: CompactionOptions,
        system_prompt: Option<String>,
        max_steps: usize,
        initial_messages: Vec<Message>,
        approve_operation: Option<Box<ApproveFn>>,
        observer: Option<Arc<dyn crate::extensions::AgentObserver>>,
    ) -> Self {
        Self {
            provider,
            observer,
            reasoning: ReasoningLevel::default(),
            tools,
            compaction,
            system_prompt: system_prompt.unwrap_or_else(|| "You are a helpful coding agent.".to_string()),
            max_steps,
            approve_operation,
            messages: initial_messages,
            last_compaction_metrics: None,
        }
    }

    pub fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub fn set_reasoning(&mut self, level: ReasoningLevel) {
        self.reasoning = level;
    }

    pub fn reasoning(&self) -> ReasoningLevel {
        self.reasoning
    }

    fn provider_messages(&self) -> Vec<Message> {
        [Message::system(self.system_prompt.clone())].into_iter().chain(self.messages.clone()).collect()
    }

    pub async fn compact(&mut self, keep_messages: usize) -> Result<bool> {
        let default_keep = self.compaction.keep_messages.unwrap_or(8);
        let keep = if keep_messages == usize::MAX { default_keep } else { keep_messages };
        if keep == 0 || self.messages.len() <= keep {
            return Ok(false);
        }
        let original = self.messages.clone();
        let split_at = original.len() - keep;
        let older = &original[..split_at];
        let transcript: String = {
            let mut lines: Vec<String> = Vec::new();
            for message in older {
                let mut line = format!("{}: {}", message.role.as_str(), message.content);
                if !message.tool_calls.is_empty() {
                    line.push_str(&format!("\ntool calls: {}", serde_json::to_string(&message.tool_calls)?));
                }
                lines.push(line);
            }
            let joined = lines.join("\n\n");
            joined.chars().skip(joined.chars().count().saturating_sub(200_000)).collect()
        };
        let estimated_tokens_before = serde_json::to_string(older)?.len().div_ceil(4);
        let mut summary = String::new();
        let mut summary_source = "fallback";
        let prompt = self.compaction.summary_prompt.clone().unwrap_or_else(|| DEFAULT_SUMMARY_PROMPT.to_string());
        for _attempt in 0..2 {
            if !summary.trim().is_empty() {
                break;
            }
            let mut request = ProviderRequest { messages: vec![Message::system(prompt.clone()), Message::user(transcript.clone())], tools: Vec::new(), model: None, reasoning: None };
            request.model = self.compaction.summary_model.clone();
            let mut stream = self.provider.stream(request);
            while let Some(event) = stream.next().await {
                if let ProviderEvent::TextDelta { text } = event? {
                    summary.push_str(&text);
                }
            }
        }
        let max_summary_chars = self.compaction.summary_max_chars.unwrap_or(12_000);
        if !summary.trim().is_empty() {
            summary = summary.trim().chars().take(max_summary_chars).collect();
            summary_source = "model";
        }
        if summary.trim().is_empty() {
            // Deterministic fallback for providers that reject summarization.
            let fallback: String = older.iter().map(|message| format!("{}: {}", message.role.as_str(), message.content)).collect::<Vec<_>>().join("\n");
            summary = format!("Conversation summary:\n{fallback}").chars().take(max_summary_chars).collect();
        }
        let keep_from_original_index = split_at;
        let metrics = CompactionMetrics {
            estimated_tokens_before,
            summary_chars: summary.chars().count(),
            summary_source,
            summary_model: self.compaction.summary_model.clone(),
        };
        self.last_compaction_metrics = Some(metrics);
        if let Some(callback) = &self.compaction.on_compaction {
            callback(&summary, keep_from_original_index);
        }
        self.messages = [Message::system(summary)].into_iter().chain(original[split_at..].to_vec()).collect();
        Ok(true)
    }

    pub async fn run_checked<F: FnMut(HarnessEvent) + Send>(&mut self, prompt: &str, on_event: F, images: Vec<ImageAttachment>, cancel: Option<Arc<AtomicBool>>) -> Result<AgentRunResult> {
        if matches!(&cancel, Some(flag) if flag.load(Ordering::SeqCst)) {
            bail!("Request cancelled");
        }
        self.run_inner(prompt, on_event, images, move || matches!(&cancel, Some(flag) if flag.load(Ordering::SeqCst))).await
    }

    pub async fn run<F: FnMut(HarnessEvent)>(&mut self, prompt: &str, on_event: F, images: Vec<ImageAttachment>) -> Result<AgentRunResult> {
        self.run_inner(prompt, on_event, images, move || false).await
    }

    async fn run_inner<F: FnMut(HarnessEvent), C: Fn() -> bool + Send + Sync>(&mut self, prompt: &str, on_event: F, images: Vec<ImageAttachment>, cancelled: C) -> Result<AgentRunResult> {
        let mut on_event = on_event;
        let turn_start = self.messages.len();
        let mut user_message = Message::user(prompt.to_string());
        user_message.images = images;
        self.messages.push(user_message.clone());
        on_event(HarnessEvent::UserMessage { message: user_message });

        let mut compacted = false;
        for step in 1..=self.max_steps {
            if cancelled() {
                bail!("Request cancelled");
            }
            let max_messages = self.compaction.max_messages.unwrap_or(24);
            let keep_messages = self.compaction.keep_messages.unwrap_or(8);
            let context_window = self.compaction.context_window.or(self.provider.config().context_window);
            let estimated_tokens = serde_json::to_string(&self.messages)?.len().div_ceil(4);
            let over_context = context_window.map(|window| estimated_tokens as u64 > window * 3 / 4).unwrap_or(false);
            if !compacted && (self.messages.len() > max_messages || over_context) && keep_messages > 0 && keep_messages < self.messages.len() {
                self.compact(usize::MAX).await?;
                compacted = true;
            }
            let plan = create_cache_plan(
                self.provider.config().id.as_str(),
                &self.provider.config().model,
                &CachePrefix {
                    system: self.system_prompt.clone(),
                    tools: self.tools.iter().map(|tool| tool.declaration().clone()).collect(),
                    context: Vec::new(),
                    resources: Vec::new(),
                    transcript: self.messages[..turn_start.min(self.messages.len())].iter().map(|message| serde_json::to_string(message).unwrap_or_default()).collect(),
                },
                &serde_json::to_string(&self.messages[turn_start.min(self.messages.len())..])?,
                true,
            );
            on_event(HarnessEvent::CachePlan { plan: plan.clone() });

            let request = ProviderRequest {
                messages: self.provider_messages(),
                tools: self.tools.iter().map(|tool| tool.declaration().clone()).collect(),
                model: None,
                reasoning: Some(self.reasoning),
            };
            let request_started = Instant::now();
            let mut text = String::new();
            let mut pending: BTreeMap<usize, PendingCall> = BTreeMap::new();
            {
                let mut stream = self.provider.stream(request);
                while let Some(event) = stream.next().await {
                    match event? {
                        ProviderEvent::TextDelta { text: delta } => {
                            text.push_str(&delta);
                            on_event(HarnessEvent::AssistantDelta { text: delta });
                        }
                        ProviderEvent::Usage { prompt_tokens, completion_tokens, cached_tokens } => {
                            on_event(HarnessEvent::Usage {
                                prompt_tokens,
                                completion_tokens,
                                cached_tokens,
                                prefix_hash: Some(plan.prefix_hash.clone()),
                                prefix_tokens: Some(plan.estimated_prefix_tokens),
                                suffix_tokens: Some(plan.estimated_suffix_tokens),
                                latency_ms: Some(request_started.elapsed().as_millis() as u64),
                            });
                        }
                        ProviderEvent::ToolCallDelta { index, id, name, arguments_delta } => {
                            let entry = pending.entry(index).or_insert_with(|| PendingCall { id: String::new(), name: String::new(), arguments: String::new() });
                            if let Some(id) = id {
                                entry.id = id;
                            }
                            if let Some(name) = name {
                                entry.name = name;
                            }
                            if let Some(arguments_delta) = arguments_delta {
                                entry.arguments.push_str(&arguments_delta);
                            }
                        }
                        ProviderEvent::Done { .. } => {}
                    }
                }
            }

            let calls: Vec<ToolCall> = pending
                .into_iter()
                .enumerate()
                .map(|(call_index, (_, call))| {
                    let arguments: Map<String, Value> = if call.arguments.trim().is_empty() {
                        Map::new()
                    } else {
                        match serde_json::from_str::<Value>(&call.arguments)? {
                            Value::Object(map) => map,
                            _ => bail!("not an object"),
                        }
                    };
                    let label = if call.name.is_empty() { format!("tool {call_index}") } else { call.name.clone() };
                    Ok(ToolCall { id: if call.id.is_empty() { format!("call_{call_index}") } else { call.id }, name: label, arguments })
                })
                .collect::<Result<Vec<ToolCall>>>()?;

            if calls.is_empty() {
                let assistant_message = Message::assistant(text.clone());
                self.messages.push(assistant_message.clone());
                on_event(HarnessEvent::AssistantMessage { message: assistant_message });
                return Ok(AgentRunResult { text, steps: step });
            }

            let mut assistant_message = Message::assistant(text.clone());
            assistant_message.tool_calls = calls.clone();
            self.messages.push(assistant_message.clone());
            on_event(HarnessEvent::AssistantMessage { message: assistant_message });

            for call in calls {
                if cancelled() {
                    bail!("Request cancelled");
                }
                on_event(HarnessEvent::ToolStart { call: call.clone() });
                let tool = self.tools.iter().find(|candidate| candidate.declaration().name == call.name).cloned();
                let result: Result<String> = async {
                    let tool = tool.ok_or_else(|| anyhow!("Unknown tool: {}", call.name))?;
                    if let Some(approve) = &self.approve_operation {
                        let request = operation_for(tool.as_ref(), &Value::Object(call.arguments.clone()));
                        on_event(HarnessEvent::ToolApprovalRequired { tool: request.tool.clone(), risk: request.risk.as_str().to_string(), summary: request.summary.clone() });
                        let approval = approve(request).await;
                        if approval.decision != "allow" {
                            bail!("Operation denied by user: {}", call.name);
                        }
                    }
                    let mut effective_args = call.arguments.clone();
                    if let Some(observer) = &self.observer {
                        let hook = observer.before_tool(&call).await.unwrap_or(Value::Object(serde_json::Map::new()));
                        if hook.get("block").and_then(Value::as_bool).unwrap_or(false) {
                            let reason = hook["reason"].as_str().map(str::to_string).unwrap_or_else(|| format!("Operation blocked by extension: {}", tool.declaration().name));
                            bail!("{reason}");
                        }
                        if let Some(Value::Object(args)) = hook.get("args") {
                            effective_args = args.clone();
                        }
                    }
                    let output = tool.execute(&Value::Object(effective_args.clone())).await;
                    if let Some(observer) = &self.observer {
                        let error_state = output.is_err();
                        let text = match &output {
                            Ok(value) => value.clone(),
                            Err(error) => format!("{error}"),
                        };
                        let transformed = observer.after_tool(&call, &text, error_state).await.ok().flatten();
                        if let Some(transformed) = transformed.filter(|value| !value.is_empty()) {
                            if output.is_ok() {
                                return Ok(transformed);
                            } else {
                                return Ok(transformed);
                            }
                        }
                    }
                    output
                }
                .await;
                match result {
                    Ok(output) => {
                        self.messages.push(Message::tool(output.clone(), call.id.clone(), call.name.clone()));
                        on_event(HarnessEvent::ToolResult { call, output, error: None });
                    }
                    Err(error) => {
                        let message = format!("Error: {error}");
                        self.messages.push(Message::tool(message.clone(), call.id.clone(), call.name.clone()));
                        on_event(HarnessEvent::ToolResult { call, output: error.to_string(), error: Some(true) });
                        let _ = message;
                    }
                }
            }
        }
        bail!("Agent stopped after {} tool steps", self.max_steps)
    }
}
