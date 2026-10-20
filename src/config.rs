//! Configuration, layered settings hierarchy, and project trust.
//! Ported from `src/config.ts` — including the hl-harness-over-Pi settings priority.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct HarnessConfig {
    pub cwd: PathBuf,
    pub resource_dirs: ResourceDirs,
    pub pi_config_dir: PathBuf,
}

#[derive(Clone, Debug)]
pub struct ResourceDirs {
    pub extensions: PathBuf,
    pub skills: PathBuf,
    pub prompts: PathBuf,
    pub themes: PathBuf,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HarnessSettings {
    pub default_provider: Option<String>,
    pub default_model: Option<String>,
    pub endpoint: Option<String>,
    pub api_key_env: Option<String>,
    pub context_window: Option<f64>,
    pub theme: Option<String>,
    pub default_thinking_level: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustedProject {
    pub path: String,
    pub fingerprint: String,
    pub trusted_at: String,
}

/// Settings layers, highest priority first: project hl-harness, personal hl-harness,
/// project Pi, personal Pi. Missing files are omitted.
pub fn settings_layers(cwd: &Path, config_dir: &Path) -> Vec<PathBuf> {
    [
        cwd.join(".harness").join("settings.json"),
        config_dir.join("harness-settings.json"),
        cwd.join(".pi").join("settings.json"),
        config_dir.join("settings.json"),
    ]
    .into_iter()
    .filter(|file| file.exists())
    .collect()
}

fn read_settings_file(file: &Path) -> serde_json::Map<String, Value> {
    std::fs::read_to_string(file)
        .ok()
        .and_then(|content| serde_json::from_str::<Value>(&content).ok())
        .and_then(|value| value.as_object().cloned())
        .unwrap_or_default()
}

/// Merge settings layers; the first layer that defines a key wins.
pub fn load_settings(cwd: &Path, config_dir: &Path) -> (HarnessSettings, Vec<PathBuf>) {
    let mut settings = HarnessSettings::default();
    let mut assign = |settings: &mut HarnessSettings, values: &serde_json::Map<String, Value>| {
        if settings.default_provider.is_none() {
            settings.default_provider = values.get("defaultProvider").and_then(Value::as_str).map(str::to_string);
        }
        if settings.default_model.is_none() {
            settings.default_model = values.get("defaultModel").and_then(Value::as_str).map(str::to_string);
        }
        if settings.endpoint.is_none() {
            settings.endpoint = values.get("endpoint").and_then(Value::as_str).map(str::to_string);
        }
        if settings.api_key_env.is_none() {
            settings.api_key_env = values.get("apiKeyEnv").and_then(Value::as_str).map(str::to_string);
        }
        if settings.context_window.is_none() {
            settings.context_window = values.get("contextWindow").and_then(Value::as_f64).filter(|window| *window > 0.0);
        }
        if settings.theme.is_none() {
            settings.theme = values.get("theme").and_then(Value::as_str).map(str::to_string);
        }
        if settings.default_thinking_level.is_none() {
            settings.default_thinking_level = values.get("defaultThinkingLevel").and_then(Value::as_str).map(str::to_string);
        }
    };
    let mut sources = Vec::new();
    for file in settings_layers(cwd, config_dir) {
        let values = read_settings_file(&file);
        assign(&mut settings, &values);
        sources.push(file);
    }
    (settings, sources)
}

pub fn load_config() -> HarnessConfig {
    default_config(&std::env::current_dir().unwrap_or_default(), &home_dir())
}

pub fn default_config(cwd: &Path, home: &Path) -> HarnessConfig {
    HarnessConfig {
        cwd: cwd.to_path_buf(),
        resource_dirs: ResourceDirs {
            extensions: cwd.join("extensions"),
            skills: cwd.join("skills"),
            prompts: cwd.join("prompts"),
            themes: cwd.join("themes"),
        },
        pi_config_dir: home.join(".pi").join("agent"),
    }
}

pub fn home_dir() -> PathBuf {
    std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/"))
}

fn trusted_projects_file(config_dir: &Path) -> PathBuf {
    config_dir.join("trusted-projects.json")
}


/// Mirror TypeScript `path.resolve`: absolute path without resolving symlinks.
fn resolve_path(path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    }
}

pub fn project_fingerprint(cwd: &Path) -> String {
    let resolved = resolve_path(cwd);
    let mut hasher = Sha256::new();
    hasher.update(resolved.to_string_lossy().as_bytes());
    hasher.finalize()[..8].iter().map(|byte| format!("{byte:02x}")).collect()
}

pub fn list_trusted_projects(config_dir: &Path) -> Vec<TrustedProject> {
    let Ok(content) = std::fs::read_to_string(trusted_projects_file(config_dir)) else { return Vec::new() };
    serde_json::from_str(&content).unwrap_or_default()
}

pub fn set_project_trust(config_dir: &Path, cwd: &Path, trusted: bool) -> Result<TrustedProject> {
    let path = resolve_path(cwd).to_string_lossy().to_string();
    let mut projects: Vec<TrustedProject> = list_trusted_projects(config_dir).into_iter().filter(|project| project.path != path).collect();
    let record = TrustedProject {
        path: path.clone(),
        fingerprint: project_fingerprint(cwd),
        trusted_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    };
    if trusted {
        projects.push(record.clone());
    }
    std::fs::create_dir_all(config_dir)?;
    std::fs::write(trusted_projects_file(config_dir), serde_json::to_string_pretty(&projects)? + "\n")?;
    Ok(record)
}

pub fn is_project_trusted(cwd: &Path, config_dir: &Path) -> bool {
    if std::env::var("HARNESS_TRUST_PROJECT").as_deref() == Ok("1") {
        return true;
    }
    let path = resolve_path(cwd).to_string_lossy().to_string();
    let fingerprint = project_fingerprint(cwd);
    list_trusted_projects(config_dir).iter().any(|project| project.path == path && project.fingerprint == fingerprint)
}
