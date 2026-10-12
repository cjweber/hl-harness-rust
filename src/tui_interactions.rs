//! Interactive MCP elicitation/sampling answered through the TUI prompt line,
//! ported from the TS `mcpInteractions()` + `parseElicitValue()` in `src/cli.ts`.

use anyhow::{anyhow, bail, Ok, Result};
use serde_json::{json, Value};
use std::sync::Arc;

use crate::tui::terminal::TerminalUi;

static ACTIVE_UI: std::sync::OnceLock<std::sync::Mutex<Option<Arc<TerminalUi>>>> = std::sync::OnceLock::new();

pub fn set_active_ui(ui: Option<Arc<TerminalUi>>) {
    let cell = ACTIVE_UI.get_or_init(|| std::sync::Mutex::new(None));
    *cell.lock().unwrap() = ui;
}

async fn ask(prompt: &str) -> Option<String> {
    let ui = ACTIVE_UI.get_or_init(|| std::sync::Mutex::new(None)).lock().unwrap().clone();
    let Some(ui) = ui else { return None };
    ui.push_transcript(format!("system: {prompt}"));
    ui.request_input(">").await.ok().flatten()
}

pub fn parse_elicit_value(schema: &Value, answer: &str) -> Result<Value> {
    let enum_values: Vec<&str> = schema["enum"].as_array().map(|items| items.iter().filter_map(Value::as_str).collect()).unwrap_or_default();
    if !enum_values.is_empty() && !enum_values.contains(&answer) {
        bail!("not one of {enum_values:?}");
    }
    match schema["type"].as_str() {
        Some("number") => {
            let value = answer.parse::<f64>().map_err(|_| anyhow!("{answer:?} is not a number"))?;
            Ok(json!(value))
        }
        Some("boolean") => Ok(json!(matches!(answer.to_lowercase().as_str(), "y" | "yes" | "true" | "1"))),
        Some("array") => Ok(json!(answer.split(',').map(|item| item.trim().to_string()).filter(|item| !item.is_empty()).collect::<Vec<_>>())),
        _ => Ok(json!(answer)),
    }
}

pub struct TuiInteractions;

impl crate::mcp::client::McpInteractions for TuiInteractions {
    fn elicit(&self, request: Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send>> {
        Box::pin(async move {
            let message = request["message"].as_str().unwrap_or("MCP elicitation");
            let Some(schema) = request.get("requestedSchema").filter(|value| value.is_object()) else {
                if request["url"].is_string() {
                    eprintln!("MCP requested URL elicitation: {}", request["url"].as_str().unwrap_or(""));
                }
                return Ok(json!({"action": "decline"}));
            };
            let mut content = serde_json::Map::new();
            for (name, schema) in schema["properties"].as_object().cloned().unwrap_or_default() {
                let hint = [schema["description"].as_str().unwrap_or_default().to_string(), schema["enum"].as_array().map(|items| format!("({})", items.iter().filter_map(Value::as_str).collect::<Vec<_>>().join("/"))).unwrap_or_default()]
                    .iter()
                    .filter(|part| !part.is_empty())
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(" ");
                let prompt = format!("MCP {name}{}:", if hint.is_empty() { String::new() } else { format!(" — {hint}") });
                let Some(answer) = ask(&prompt).await else { return Ok(json!({"action": "cancel"})) };
                match std::result::Result::<_, anyhow::Error>::from(parse_elicit_value(&schema, &answer)) {
                    std::result::Result::Ok(value) => {
                        content.insert(name.clone(), value);
                    }
                    std::result::Result::Err(_) => return Ok(json!({"action": "decline"})),
                }
            }
            Ok(json!({"action": "accept", "content": content}))
        })
    }

    fn sample(&self, request: Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send>> {
        Box::pin(async move {
            let prompt: String = request["messages"]
                .as_array()
                .map(|messages| {
                    messages
                        .iter()
                        .map(|message| {
                            let content = &message["content"];
                            format!("{}: {}", message["role"].as_str().unwrap_or("?"), content.as_str().unwrap_or(&content.to_string()))
                        })
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .unwrap_or_default()
                .chars()
                .rev()
                .take(2000)
                .collect::<String>()
                .chars()
                .rev()
                .collect();
            let model = request["modelPreferences"]["hints"][0]["name"].as_str().unwrap_or("harness").to_string();
            let Some(answer) = ask(&format!("MCP requests a model response (prefers {model}):\n{prompt}")).await else {
                bail!("Sampling cancelled by user");
            };
            Ok(json!({"role": "assistant", "content": {"type": "text", "text": answer}, "model": model, "stopReason": "endTurn"}))
        })
    }
}
