//! Deterministic cache-prefix planning, ported from `src/cache/prefix.ts`.
//!
//! The canonical serialization must stay byte-identical with the TypeScript
//! implementation so prefix hashes match across runtimes.

use crate::types::{CachePlan, ToolDeclaration};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Default)]
pub struct CachePrefix {
    pub system: String,
    pub tools: Vec<ToolDeclaration>,
    pub context: Vec<String>,
    pub resources: Vec<String>,
    pub transcript: Vec<String>,
}

/// Canonical JSON: object keys sorted, no whitespace — mirrors the TS `canonical()`.
pub fn canonical(value: &Value) -> String {
    match value {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => serde_json::to_string(s).unwrap_or_default(),
        Value::Array(items) => {
            let parts: Vec<String> = items.iter().map(canonical).collect();
            format!("[{}]", parts.join(","))
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let parts: Vec<String> = keys
                .iter()
                .map(|key| format!("{}:{}", serde_json::to_string(key).unwrap_or_default(), canonical(&map[*key])))
                .collect();
            format!("{{{}}}", parts.join(","))
        }
    }
}

pub fn serialize_prefix(prefix: &CachePrefix) -> String {
    let mut tools = prefix.tools.clone();
    tools.sort_by(|a, b| a.name.cmp(&b.name));
    let mut context = prefix.context.clone();
    context.sort();
    let mut resources = prefix.resources.clone();
    resources.sort();
    canonical(&json!({
        "system": prefix.system,
        "tools": tools,
        "context": context,
        "resources": resources,
        "transcript": prefix.transcript,
    }))
}

pub fn create_cache_plan(provider: &str, model: &str, prefix: &CachePrefix, suffix: &str, cacheable: bool) -> CachePlan {
    let prefix_text = serialize_prefix(prefix);
    let prefix_bytes = prefix_text.as_bytes().len();
    let suffix_bytes = suffix.as_bytes().len();
    let mut hasher = Sha256::new();
    hasher.update(prefix_text.as_bytes());
    let prefix_hash = hex(&hasher.finalize());
    CachePlan {
        provider: provider.to_string(),
        model: model.to_string(),
        prefix_hash,
        prefix_bytes,
        suffix_bytes,
        estimated_prefix_tokens: prefix_bytes.div_ceil(4),
        estimated_suffix_tokens: suffix_bytes.div_ceil(4),
        cacheable,
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn canonical_matches_typescript_output() {
        assert_eq!(canonical(&json!({"b": 1, "a": [true, null, "x"]})), r#"{"a":[true,null,"x"],"b":1}"#);
    }

    #[test]
    fn plan_hashes_the_sorted_prefix() {
        let mut prefix = CachePrefix { system: "s".into(), ..Default::default() };
        prefix.tools.push(ToolDeclaration { name: "b".into(), description: String::new(), parameters: json!({}) });
        prefix.tools.push(ToolDeclaration { name: "a".into(), description: String::new(), parameters: json!({}) });
        let plan = create_cache_plan("openrouter", "m", &prefix, "suffix", true);
        assert_eq!(plan.prefix_hash.len(), 64);
        assert!(plan.cacheable);
        // Sorted tools change nothing relative to the reverse order.
        let mut reversed = prefix.clone();
        reversed.tools.reverse();
        assert_eq!(create_cache_plan("openrouter", "m", &reversed, "suffix", true).prefix_hash, plan.prefix_hash);
    }
}
