//! Provider presets and construction, ported from `src/model/provider-factory.ts`.

use super::anthropic::AnthropicProvider;
use super::provider::{OpenAICompatibleProvider, Provider, ProviderConfig, ProviderEvent, ProviderKind, ProviderRequest};
use async_trait::async_trait;
use futures_util::stream::BoxStream;
use futures_util::StreamExt;
use serde_json::Value;
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
    /// Ordered fallback providers, tried when the primary fails early.
    pub fallbacks: Vec<ProviderOptions>,
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

/// The default API-key environment variable for a provider kind.
pub fn api_key_env_for(id: ProviderKind) -> &'static str {
    preset(id).key_env
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

/// Parse fallback entries from settings: objects ({provider, model, endpoint,
/// apiKeyEnv}) or plain strings treated as a model override on the primary's
/// provider kind.
pub fn parse_fallbacks(values: &[Value]) -> Vec<ProviderOptions> {
    values
        .iter()
        .filter_map(|value| {
            let mut options = ProviderOptions::default();
            match value {
                Value::String(text) => options.model = Some(text.clone()),
                Value::Object(map) => {
                    if let Some(provider) = map.get("provider").and_then(Value::as_str) {
                        options.provider = ProviderKind::from_str(provider);
                    }
                    if let Some(model) = map.get("model").and_then(Value::as_str) {
                        options.model = Some(model.to_string());
                    }
                    if let Some(endpoint) = map.get("endpoint").and_then(Value::as_str) {
                        options.endpoint = Some(endpoint.to_string());
                    }
                    if let Some(key_env) = map.get("apiKeyEnv").and_then(Value::as_str) {
                        options.api_key_env = Some(key_env.to_string());
                    }
                }
                _ => return None,
            }
            if options.provider.is_none() && options.model.is_none() {
                return None;
            }
            Some(options)
        })
        .collect()
}

pub fn create_provider(options: &ProviderOptions) -> anyhow::Result<Arc<dyn Provider>> {
    let primary = create_single_provider(options)?;
    if options.fallbacks.is_empty() {
        return Ok(primary);
    }
    let mut fallbacks = Vec::new();
    for fallback in &options.fallbacks {
        // Unspecified fallback fields inherit the primary's provider kind.
        let mut fallback = fallback.clone();
        if fallback.provider.is_none() {
            fallback.provider = options.provider;
        }
        fallbacks.push(create_single_provider(&fallback)?);
    }
    Ok(Arc::new(FallbackProvider::new(primary, fallbacks)))
}

fn create_single_provider(options: &ProviderOptions) -> anyhow::Result<Arc<dyn Provider>> {
    let config = provider_config(options)?;
    Ok(match config.id {
        ProviderKind::Anthropic => Arc::new(AnthropicProvider::new(config)),
        _ => Arc::new(OpenAICompatibleProvider::new(config)),
    })
}

/// Ordered fallback chain: a stream that fails before producing any event
/// falls through to the next provider; failures after the stream started
/// producing are propagated (mirrors the TS provider fallback behavior).
pub struct FallbackProvider {
    primary: Arc<dyn Provider>,
    fallbacks: Vec<Arc<dyn Provider>>,
}

impl FallbackProvider {
    pub fn new(primary: Arc<dyn Provider>, fallbacks: Vec<Arc<dyn Provider>>) -> Self {
        Self { primary, fallbacks }
    }
}

#[async_trait]
impl Provider for FallbackProvider {
    fn config(&self) -> &ProviderConfig {
        self.primary.config()
    }

    fn stream(&self, request: ProviderRequest) -> BoxStream<'_, anyhow::Result<ProviderEvent>> {
        let chain: Vec<Arc<dyn Provider>> = std::iter::once(self.primary.clone()).chain(self.fallbacks.iter().cloned()).collect();
        Box::pin(async_stream::try_stream! {
            let mut last_error: Option<anyhow::Error> = None;
            for provider in chain {
                let mut stream = provider.stream(request.clone());
                let mut produced = false;
                let mut failed_early = false;
                loop {
                    match stream.next().await {
                        Some(Ok(event)) => {
                            produced = true;
                            yield event;
                        }
                        Some(Err(error)) => {
                            if produced {
                                // Mid-stream errors after output propagate.
                                Err(anyhow::anyhow!("{error}"))?;
                            }
                            failed_early = true;
                            last_error = Some(error);
                            break;
                        }
                        None => break,
                    }
                }
                if !failed_early {
                    return;
                }
            }
            Err(last_error.unwrap_or_else(|| anyhow::anyhow!("provider fallback chain failed")))?;
        })
    }
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
