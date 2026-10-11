//! Permission gate, ported from `src/security/permissions.ts`.

use crate::tools::Tool;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionMode {
    #[serde(rename = "read-only")]
    ReadOnly,
    Ask,
    Full,
}

impl PermissionMode {
    pub fn from_str(value: &str) -> Option<Self> {
        match value {
            "read-only" => Some(Self::ReadOnly),
            "ask" => Some(Self::Ask),
            "full" => Some(Self::Full),
            _ => None,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PermissionRule {
    pub tool: String,
    pub decision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PermissionAudit {
    pub tool: String,
    pub risk: crate::types::RiskLevel,
    pub decision: &'static str,
    pub source: &'static str,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationRequest {
    pub tool: String,
    pub risk: crate::types::RiskLevel,
    pub arguments: Value,
    pub summary: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ApprovalResult {
    pub decision: &'static str, // "allow" | "deny"
    pub remember: bool,
}

impl ApprovalResult {
    pub fn allow(remember: bool) -> Self {
        Self { decision: "allow", remember }
    }

    pub fn deny() -> Self {
        Self { decision: "deny", remember: false }
    }
}

pub fn load_permission_rules(config_dir: &Path) -> Vec<PermissionRule> {
    let Ok(content) = std::fs::read_to_string(config_dir.join("permissions.json")) else { return Vec::new() };
    let Ok(serde_json::Value::Array(items)) = serde_json::from_str::<serde_json::Value>(&content) else { return Vec::new() };
    items
        .iter()
        .filter_map(|item| {
            let rule = item.as_object()?;
            let tool = rule.get("tool")?.as_str()?.to_string();
            let decision = rule.get("decision")?.as_str()?.to_string();
            if decision != "allow" && decision != "deny" {
                return None;
            }
            let scope = rule.get("scope").and_then(serde_json::Value::as_str).map(str::to_string);
            if let Some(scope) = &scope {
                if !["tool", "risk", "server"].contains(&scope.as_str()) {
                    return None;
                }
            }
            Some(PermissionRule { tool, decision, scope, value: rule.get("value").and_then(serde_json::Value::as_str).map(str::to_string) })
        })
        .collect()
}

pub fn save_permission_rule(config_dir: &Path, rule: PermissionRule) -> Result<()> {
    std::fs::create_dir_all(config_dir)?;
    let mut rules: Vec<PermissionRule> = load_permission_rules(config_dir)
        .into_iter()
        .filter(|candidate| !(candidate.tool == rule.tool && candidate.scope == rule.scope && candidate.value == rule.value))
        .collect();
    rules.push(rule);
    rules.sort_by(|a, b| a.tool.cmp(&b.tool));
    std::fs::write(config_dir.join("permissions.json"), serde_json::to_string_pretty(&rules)? + "\n")?;
    Ok(())
}

pub fn operation_for(tool: &dyn Tool, args: &Value) -> OperationRequest {
    let risk = tool.risk();
    let detail = match risk {
        crate::types::RiskLevel::Shell => args.get("command").and_then(serde_json::Value::as_str),
        crate::types::RiskLevel::Network => args.get("url").and_then(serde_json::Value::as_str),
        crate::types::RiskLevel::Write => args.get("path").and_then(serde_json::Value::as_str),
        _ => None,
    }
    .map(str::to_string)
    .unwrap_or_else(|| serde_json::to_string(args).unwrap_or_default());
    let summary = format!("{}: {}", tool.declaration().name, detail);
    OperationRequest { tool: tool.declaration().name.clone(), risk, arguments: args.clone(), summary: summary.chars().take(500).collect() }
}

/// Mode + rules + always-allow set: the stateful half of the TS `PermissionGate`.
#[derive(Clone, Debug)]
pub struct GatePolicy {
    pub mode: PermissionMode,
    pub rules: Vec<PermissionRule>,
    pub always_allow: HashSet<String>,
}

impl GatePolicy {
    pub fn new(mode: PermissionMode, config_dir: &Path, always_allow: Vec<String>) -> Self {
        Self { mode, rules: load_permission_rules(config_dir), always_allow: always_allow.into_iter().collect() }
    }

    fn matched_rule(&self, request: &OperationRequest) -> Option<&PermissionRule> {
        self.rules.iter().find(|candidate| {
            if candidate.tool == "*" {
                true
            } else if candidate.scope.as_deref() == Some("risk") {
                candidate.value.as_deref() == Some(request.risk.as_str())
            } else if candidate.scope.as_deref() == Some("server") {
                let server = request.tool.split(':').nth(1).unwrap_or_default();
                let server_value = candidate.value.clone().unwrap_or_default();
                candidate.value.as_deref() == Some(server) || request.tool.starts_with(&format!("mcp__{server_value}__"))
            } else {
                candidate.tool == request.tool
            }
        })
    }

    fn audit(&self, request: &OperationRequest, decision: &'static str, source: &'static str) -> PermissionAudit {
        // Telemetry failures must not change authorization; no unwraps here.
        PermissionAudit { tool: request.tool.clone(), risk: request.risk, decision, source }
    }

    /// Resolve the decision without an interactive prompt.
    pub fn authorize(&self, request: &OperationRequest) -> PermissionAudit {
        if self.always_allow.contains(&request.tool) || self.always_allow.contains("*") {
            return self.audit(request, "allow", "always-allow");
        }
        if let Some(rule) = self.matched_rule(request) {
            let allow = rule.decision == "allow";
            return self.audit(request, if allow { "allow" } else { "deny" }, "rule");
        }
        if self.mode == PermissionMode::Full || request.risk == crate::types::RiskLevel::Safe {
            return self.audit(request, "allow", "mode");
        }
        self.audit(request, "deny", "unsafe-default")
    }

    pub fn allows(&self, request: &OperationRequest) -> bool {
        self.authorize(request).decision == "allow"
    }

    pub async fn check(&self, tool: &dyn Tool, args: &Value) -> bool {
        self.allows(&operation_for(tool, args))
    }
}

impl Default for GatePolicy {
    fn default() -> Self {
        Self { mode: PermissionMode::ReadOnly, rules: Vec::new(), always_allow: HashSet::new() }
    }
}

pub type SharedGatePolicy = Arc<GatePolicy>;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::builtin_tools;
    use serde_json::json;

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("harness-permissions-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn read_only_mode_denies_write_and_shell_but_allows_safe() {
        let dir = temp_dir();
        let tools = builtin_tools(&dir);
        let bash = tools.iter().find(|tool| tool.declaration().name == "bash").unwrap().clone();
        let gate = GatePolicy::new(PermissionMode::ReadOnly, &dir, Vec::new());
        let request = operation_for(bash.as_ref(), &json!({"command": "ls"}));
        assert_eq!(gate.authorize(&request).decision, "deny");
        assert_eq!(gate.authorize(&request).source, "unsafe-default");
        let read = tools.iter().find(|tool| tool.declaration().name == "read").unwrap().clone();
        assert!(gate.check(read.as_ref(), &json!({"path": "README.md"})).await);
    }

    #[test]
    fn rules_persist_and_are_sorted() {
        let dir = temp_dir();
        save_permission_rule(&dir, PermissionRule { tool: "bash".into(), decision: "allow".into(), scope: None, value: None }).unwrap();
        save_permission_rule(&dir, PermissionRule { tool: "edit".into(), decision: "allow".into(), scope: Some("risk".into()), value: Some("write".into()) }).unwrap();
        let rules = load_permission_rules(&dir);
        assert_eq!(rules.len(), 2);
        assert_eq!(rules[0].tool, "bash");
    }

    #[test]
    fn operations_summarize_by_risk() {
        let dir = temp_dir();
        let tools: Vec<std::sync::Arc<dyn Tool>> = builtin_tools(&dir);
        let bash = tools.iter().find(|tool| tool.declaration().name == "bash").unwrap();
        let request = operation_for(bash.as_ref(), &json!({"command": "grep -c a file"}));
        assert_eq!(request.summary, "bash: grep -c a file");
        assert_eq!(request.risk, crate::types::RiskLevel::Shell);
    }

    #[tokio::test]
    async fn full_mode_allows_everything_and_rules_override() {
        let dir = temp_dir();
        let tools = builtin_tools(&dir);
        let bash = tools.iter().find(|tool| tool.declaration().name == "bash").unwrap().clone();
        let full = GatePolicy::new(PermissionMode::Full, &dir, Vec::new());
        assert!(full.check(bash.as_ref(), &json!({"command": "ls"})).await);
        save_permission_rule(&dir, PermissionRule { tool: "bash".into(), decision: "deny".into(), scope: None, value: None }).unwrap();
        let gated = GatePolicy::new(PermissionMode::Full, &dir, Vec::new());
        let request = operation_for(bash.as_ref(), &json!({"command": "ls"}));
        assert_eq!(gated.authorize(&request).decision, "deny");
        assert_eq!(gated.authorize(&request).source, "rule");
    }

    #[tokio::test]
    async fn always_allow_overrides_rules() -> anyhow::Result<()> {
        let dir = temp_dir();
        let tools = builtin_tools(&dir);
        let bash = tools.iter().find(|tool| tool.declaration().name == "bash").unwrap().clone();
        save_permission_rule(&dir, PermissionRule { tool: "bash".into(), decision: "deny".into(), scope: None, value: None }).unwrap();
        let gate = GatePolicy::new(PermissionMode::ReadOnly, &dir, vec!["bash".into()]);
        assert!(gate.check(bash.as_ref(), &json!({"command": "ls"})).await);
        Ok(())
    }
}
