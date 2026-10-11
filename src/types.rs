//! Core data model, ported 1:1 from the TypeScript harness `src/types.ts`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}


#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RiskLevel {
    Safe,
    Write,
    Shell,
    Network,
    Extension,
    Mcp,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::System => "system",
            Self::User => "user",
            Self::Assistant => "assistant",
            Self::Tool => "tool",
        }
    }
}

impl RiskLevel {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Safe => "safe",
            Self::Write => "write",
            Self::Shell => "shell",
            Self::Network => "network",
            Self::Extension => "extension",
            Self::Mcp => "mcp",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: Map<String, Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageAttachment {
    pub mime_type: String,
    pub data: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Message {
    pub role: Role,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ImageAttachment>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
}

impl Message {
    pub fn user(content: impl Into<String>) -> Self {
        Self { role: Role::User, content: content.into(), images: Vec::new(), tool_call_id: None, tool_name: None, tool_calls: Vec::new() }
    }

    pub fn system(content: impl Into<String>) -> Self {
        Self { role: Role::System, content: content.into(), images: Vec::new(), tool_call_id: None, tool_name: None, tool_calls: Vec::new() }
    }

    pub fn tool(content: impl Into<String>, call_id: impl Into<String>, name: impl Into<String>) -> Self {
        Self { role: Role::Tool, content: content.into(), images: Vec::new(), tool_call_id: Some(call_id.into()), tool_name: Some(name.into()), tool_calls: Vec::new() }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self { role: Role::Assistant, content: content.into(), images: Vec::new(), tool_call_id: None, tool_name: None, tool_calls: Vec::new() }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolDeclaration {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CachePlan {
    pub provider: String,
    pub model: String,
    pub prefix_hash: String,
    pub prefix_bytes: usize,
    pub suffix_bytes: usize,
    pub estimated_prefix_tokens: usize,
    pub estimated_suffix_tokens: usize,
    pub cacheable: bool,
}

/// Harness events, serialized exactly like the TypeScript `HarnessEvent` union
/// (`{"type":"assistant_delta","text":"..."}` with camelCase fields).
#[derive(Clone, Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum HarnessEvent {
    UserMessage { message: Message },
    AssistantDelta { text: String },
    AssistantMessage { message: Message },
    ToolStart { call: ToolCall },
    ToolResult { call: ToolCall, output: String, #[serde(skip_serializing_if = "Option::is_none")] error: Option<bool> },
    ToolApprovalRequired { tool: String, risk: String, summary: String },
    CachePlan { plan: CachePlan },
    Usage {
        prompt_tokens: u64,
        completion_tokens: u64,
        cached_tokens: u64,
        #[serde(skip_serializing_if = "Option::is_none")] prefix_hash: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")] prefix_tokens: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")] suffix_tokens: Option<usize>,
        #[serde(skip_serializing_if = "Option::is_none")] latency_ms: Option<u64>,
    },
    Error { message: String },
    Status { text: String },
}

impl HarnessEvent {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::UserMessage { .. } => "user_message",
            Self::AssistantDelta { .. } => "assistant_delta",
            Self::AssistantMessage { .. } => "assistant_message",
            Self::ToolStart { .. } => "tool_start",
            Self::ToolResult { .. } => "tool_result",
            Self::ToolApprovalRequired { .. } => "tool_approval_required",
            Self::CachePlan { .. } => "cache_plan",
            Self::Usage { .. } => "usage",
            Self::Error { .. } => "error",
            Self::Status { .. } => "status",
        }
    }
}
