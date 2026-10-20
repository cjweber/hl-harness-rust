//! Provider presets and construction, ported from `src/model/provider-factory.ts`.

use super::anthropic::AnthropicProvider;
use super::provider::{OpenAICompatibleProvider, Provider, ProviderConfig, ProviderKind};
use std::sync::Arc;

pub type ProviderId = ProviderKind;

#[derive(Clone, Debug, Default)]
pub struct ProviderOptions {
    pub provider: Option<ProviderId>,
    pub model: Option<String>,
    pub endpoint: Option<String>,
    pub api_key: Option<String>,
    pub api_key_env: Option<String>,
    pub session_id: Option<String>,
    pub reasoning: Option<crate::types::ReasoningLevel>,
    pub timeout_ms: Option<u64>,
    pub max_retries: Option<u32>,
    pub context_window: Option<u64>,
    pub headers: Vec<(String, String)>,
}

impl ProviderOptions {
    pub fn new() -> Self {
        Self::default()
    }
}

#[allow(dead_code)]
fn _assert_default() {
    let _ = ProviderOptions::default();
}

struct Preset {
    endpoint: &'static str,
    model: &'static str,
    key_env: &'static str,
}

fn preset(id: ProviderKind) -> Preset {
    match id {
        ProviderKind::OpenRouter => Preset { endpoint: "https://openrouter.ai/api/v1", model: "openai/gpt-4o-mini", key_env: "OPENROUTER_API_KEY" },
        ProviderKind::OpenCodeGo => Preset { endpoint: "https://opencode.ai/zen/go/v1", model: "kimi-k2.6", key_env: "OPENCODE_API_KEY" },
        ProviderKind::Anthropic => Preset { endpoint: "https://api.anthropic.com/v1", model: "claude-sonnet-4-20250514", key_env: "ANTHROPIC_API_KEY" },
        ProviderKind::Custom => Preset { endpoint: "", model: "local-model", key_env: "HARNESS_API_KEY" },
    }
}

pub fn provider_config(options: &ProviderOptions) -> anyhow::Result<ProviderConfig> {
    let id = options.provider.unwrap_or(ProviderKind::OpenRouter);
    let preset = preset(id);
    let base_url = options
        .endpoint
        .clone()
        .or_else(|| if id == ProviderKind::Custom { std::env::var("HARNESS_ENDPOINT").ok() } else { None })
        .unwrap_or_else(|| preset.endpoint.to_string());
    if base_url.is_empty() {
        anyhow::bail!("Custom provider requires --endpoint or HARNESS_ENDPOINT");
    }
    let key_env = options.api_key_env.clone().unwrap_or_else(|| preset.key_env.to_string());
    let api_key = options
        .api_key
        .clone()
        .filter(|value| !value.is_empty())
        .or_else(|| std::env::var(&key_env).ok().filter(|value| !value.is_empty()));
    if id != ProviderKind::Custom && api_key.is_none() {
        anyhow::bail!("{} requires {key_env}", id.as_str());
    }
    let env_model = std::env::var(format!("{}_MODEL", id.as_str().to_uppercase().replace('-', "_")))
        .ok()
        .filter(|value| !value.is_empty());
    let model = options
        .model
        .clone()
        .or_else(|| if id == ProviderKind::Custom { std::env::var("HARNESS_MODEL").ok() } else { None })
        .or(env_model)
        .unwrap_or_else(|| preset.model.to_string());
    let env_context_window = std::env::var("HARNESS_CONTEXT_WINDOW").ok().and_then(|value| value.parse::<u64>().ok());
    Ok(ProviderConfig {
        id,
        name: id.as_str().to_string(),
        base_url,
        api_key,
        model,
        session_id: options.session_id.clone(),
        headers: options.headers.clone(),
        timeout_ms: options.timeout_ms,
        max_retries: options.max_retries,
        retry_delay_ms: None,
        context_window: options.context_window.or(env_context_window),
    })
}

pub fn create_provider(options: &ProviderOptions) -> anyhow::Result<Arc<dyn Provider>> {
    let config = provider_config(options)?;
    Ok(match config.id {
        ProviderKind::Anthropic => Arc::new(AnthropicProvider::new(config)),
        _ => Arc::new(OpenAICompatibleProvider::new(config)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presets_match_typescript_defaults() {
        let openrouter = provider_config(&ProviderOptions { provider: Some(ProviderKind::OpenRouter), model: Some("openai/test".into()), api_key: Some("or-key".into()), ..ProviderOptions::new() }).unwrap();
        assert_eq!(openrouter.base_url, "https://openrouter.ai/api/v1");
        let opencode = provider_config(&ProviderOptions { provider: Some(ProviderKind::OpenCodeGo), model: Some("kimi-k2.6".into()), api_key: Some("oc-key".into()), ..ProviderOptions::new() }).unwrap();
        assert_eq!(opencode.base_url, "https://opencode.ai/zen/go/v1");
        // Session affinity: the OpenAI-compatible provider generates the id at construction.
        let provider = create_provider(&ProviderOptions { provider: Some(ProviderKind::OpenCodeGo), api_key: Some("oc-key".into()), ..ProviderOptions::new() }).unwrap();
        assert!(provider.config().session_id.is_some());
        let custom = provider_config(&ProviderOptions { provider: Some(ProviderKind::Custom), model: Some("local".into()), endpoint: Some("http://localhost:1234/v1".into()), ..ProviderOptions::new() }).unwrap();
        assert_eq!(custom.base_url, "http://localhost:1234/v1");
        assert!(custom.api_key.is_none());
        let missing = provider_config(&ProviderOptions { provider: Some(ProviderKind::OpenCodeGo), api_key: Some(String::new()), ..ProviderOptions::new() });
        assert!(missing.is_err());
    }
}
