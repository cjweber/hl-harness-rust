//! Model discovery, ported from `src/model/discovery.ts`.

use super::provider::{ProviderConfig, ProviderKind};
use anyhow::{anyhow, Result};
use serde_json::Value;

#[derive(Clone, Debug)]
pub struct ModelInfo {
    pub id: String,
    pub name: String,
    pub provider: ProviderKind,
    pub context_window: Option<u64>,
    pub max_output_tokens: Option<u64>,
    pub supports_tools: bool,
    pub supports_vision: bool,
    pub supports_reasoning: bool,
    pub input_modalities: Vec<String>,
    pub output_modalities: Vec<String>,
}

pub async fn discover_models(config: &ProviderConfig) -> Result<Vec<ModelInfo>> {
    let url = if config.id == ProviderKind::Anthropic {
        format!("{}/models?limit=100", config.base_url.trim_end_matches('/'))
    } else {
        format!("{}/models", config.base_url.trim_end_matches('/'))
    };
    let mut request = reqwest::Client::new().get(&url).header("accept", "application/json");
    for (name, value) in &config.headers {
        request = request.header(name.as_str(), value.as_str());
    }
    if config.id == ProviderKind::Anthropic {
        request = request
            .header("x-api-key", config.api_key.clone().unwrap_or_default())
            .header("anthropic-version", "2023-06-01");
    } else if let Some(api_key) = &config.api_key {
        request = request.header("authorization", format!("Bearer {api_key}"));
    }
    let response = request.send().await?;
    if !response.status().is_success() {
        let status = response.status().as_u16();
        let body: String = response.text().await.unwrap_or_default().chars().take(500).collect();
        return Err(anyhow!("{} model discovery failed ({status}): {body}", config.name));
    }
    let value: Value = response.json().await?;
    let models = value["data"]
        .as_array()
        .or_else(|| value["models"].as_array())
        .cloned()
        .unwrap_or_default();
    Ok(models
        .iter()
        .filter_map(|model| {
            let id = model["id"].as_str().or_else(|| model["name"].as_str()).unwrap_or_default().to_string();
            if id.is_empty() {
                return None;
            }
            let parameters = model["supported_parameters"].as_array().map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect::<Vec<_>>()).unwrap_or_default();
            let default_vision = config.id == ProviderKind::Anthropic;
            let modalities = model["architecture"]["input_modalities"]
                .as_array()
                .map(|items| items.iter().filter_map(Value::as_str).map(str::to_string).collect::<Vec<_>>())
                .unwrap_or_else(|| if default_vision { vec!["text".into(), "image".into()] } else { vec!["text".into()] });
            Some(ModelInfo {
                name: model["display_name"].as_str().or_else(|| model["name"].as_str()).unwrap_or(&id).to_string(),
                id,
                provider: config.id,
                context_window: model["context_length"].as_u64().or_else(|| model["context_window"].as_u64()),
                max_output_tokens: model["max_output_tokens"].as_u64(),
                supports_tools: if parameters.is_empty() { true } else { parameters.iter().any(|p| p == "tools" || p == "tool_choice") },
                supports_vision: modalities.iter().any(|m| m == "image"),
                supports_reasoning: parameters.iter().any(|p| p == "reasoning" || p == "include_reasoning"),
                input_modalities: modalities,
                output_modalities: vec!["text".into()],
            })
        })
        .collect())
}
