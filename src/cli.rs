//! CLI, ported from `src/cli.ts` (run, models, cache, trust, settings, version).

use crate::config::{default_config, home_dir, is_project_trusted, list_trusted_projects, load_settings, project_fingerprint, set_project_trust, HarnessSettings};
use crate::core::agent::{Agent, CompactionOptions};
use crate::model::discovery::discover_models;
use crate::model::factory::{provider_config, ProviderId, ProviderOptions};
use crate::security::permissions::{ApprovalResult, GatePolicy, PermissionMode, SharedGatePolicy};
use crate::sessions::store::SessionStore;
use crate::tools::builtin_tools;
use crate::types::{HarnessEvent, ImageAttachment, Message};
use anyhow::{bail, Result};
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Parser, Clone)]
#[command(name = "hl-harness", version, about = "Rust port of the hl-harness coding-agent harness")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Subcommand, Debug, Clone)]
pub enum Commands {
    /// Run one prompt with the agent loop (non-interactive).
    Run {
        prompt: String,
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        endpoint: Option<String>,
        #[arg(long)]
        api_key_env: Option<String>,
        #[arg(long, default_value_t = 8)]
        max_steps: usize,
        #[arg(long)]
        session: Option<PathBuf>,
        #[arg(long = "image")]
        images: Vec<PathBuf>,
        #[arg(long)]
        permission_mode: Option<String>,
        /// Allow all operations for this invocation.
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
        #[arg(long, default_value_t = 60_000)]
        timeout: u64,
        #[arg(long, default_value_t = 2)]
        retries: u32,
    },
    /// Interactive TUI (not yet ported; see docs/remaining-scope.md).
    Tui,
    Models {
        #[arg(long)]
        provider: Option<String>,
        #[arg(long)]
        model: Option<String>,
        #[arg(long)]
        endpoint: Option<String>,
        #[arg(long)]
        api_key_env: Option<String>,
        #[arg(long)]
        timeout: Option<u64>,
        #[arg(long, default_value_t = 2)]
        retries: u32,
    },
    Trust {
        #[arg(default_value = "status")]
        action: String,
        #[arg(default_value = None)]
        path: Option<PathBuf>,
    },
    /// Inspect persisted per-request cache history from a session file.
    Cache {
        session: PathBuf,
        #[arg(long)]
        json: bool,
    },
    Settings,
    Version,
}

/// Precedence: CLI flags, then environment, then hl-harness settings, then Pi settings.
/// Ported from the TypeScript `applySettings()`.
pub fn apply_settings(
    cli_provider: Option<&str>,
    cli_model: Option<String>,
    cli_endpoint: Option<String>,
    cli_api_key_env: Option<String>,
) -> Result<(ProviderId, ProviderOptions)> {
    let config = default_config(&std::env::current_dir()?, &home_dir());
    let (settings, _) = load_settings(&config.cwd, &config.pi_config_dir);
    let HarnessSettings { default_provider, default_model, endpoint, api_key_env, context_window, theme: _ } = settings;
    let provider_name = cli_provider.map(str::to_string).or(default_provider).unwrap_or_else(|| "openrouter".to_string());
    let provider = ProviderId::from_str(&provider_name).ok_or_else(|| anyhow::anyhow!("Unknown provider: {provider_name}"))?;
    let env_model = std::env::var(format!("{}_MODEL", provider.as_str().to_uppercase().replace('-', "_")))
        .ok()
        .filter(|value| !value.is_empty());
    let env_window = std::env::var("HARNESS_CONTEXT_WINDOW").ok().and_then(|value| value.parse::<u64>().ok()).filter(|window| *window > 0);
    let model = cli_model.or(default_model).or(env_model);
    let endpoint = cli_endpoint.or(endpoint).or_else(|| std::env::var("HARNESS_ENDPOINT").ok().filter(|value| !value.is_empty()));
    let api_key_env = cli_api_key_env.or(api_key_env);
    let context_window = context_window.filter(|window| *window > 0.0).map(|window| window as u64).or(env_window);
    Ok((
        provider,
        ProviderOptions { provider: Some(provider), model, endpoint, api_key: None, api_key_env, session_id: None, timeout_ms: None, max_retries: None, context_window, headers: Vec::new() },
    ))
}

fn load_images(paths: &[PathBuf]) -> Result<Vec<ImageAttachment>> {
    const MIME_TYPES: [(&str, &str); 5] = [(".png", "image/png"), (".jpg", "image/jpeg"), (".jpeg", "image/jpeg"), (".gif", "image/gif"), (".webp", "image/webp")];
    paths
        .iter()
        .map(|path| {
            let extension = path.extension().and_then(|extension| extension.to_str()).unwrap_or_default().to_lowercase();
            let mime_type = MIME_TYPES.iter().find(|(candidate, _)| *candidate == format!(".{extension}")).map(|(_, mime)| mime.to_string()).ok_or_else(|| anyhow::anyhow!("Unsupported image type: {}", path.display()))?;
            let data = base64::encode(&std::fs::read(path)?);
            Ok(ImageAttachment { mime_type, data })
        })
        .collect()
}

mod base64 {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    pub fn encode(bytes: &[u8]) -> String {
        let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = *chunk.get(1).unwrap_or(&0) as u32;
            let b2 = *chunk.get(2).unwrap_or(&0) as u32;
            let bits = (b0 << 16) | (b1 << 8) | b2;
            output.push(ALPHABET[(bits >> 18) as usize & 63] as char);
            output.push(ALPHABET[(bits >> 12) as usize & 63] as char);
            output.push(if chunk.len() > 1 { ALPHABET[(bits >> 6) as usize & 63] as char } else { '=' });
            output.push(if chunk.len() > 2 { ALPHABET[bits as usize & 63] as char } else { '=' });
        }
        output
    }
}

fn print_event(event: &HarnessEvent, json_mode: bool) {
    if json_mode {
        println!("{}", serde_json::to_string(event).unwrap_or_default());
        return;
    }
    if let HarnessEvent::AssistantDelta { text } = event {
        use std::io::Write;
        print!("{text}");
        let _ = std::io::stdout().flush();
        return;
    }
    match event {
        HarnessEvent::Usage { prompt_tokens, completion_tokens, cached_tokens, latency_ms, prefix_hash, .. } => {
            let hit = if *prompt_tokens > 0 { *cached_tokens * 100 / *prompt_tokens } else { 0 };
            eprintln!(
                "\n[usage] prompt={prompt_tokens} completion={completion_tokens} cached={cached_tokens} hit={hit}%{}{}",
                latency_ms.map(|ms| format!(" latency={ms}ms")).unwrap_or_default(),
                prefix_hash.as_deref().map(|hash| format!(" prefix={}", &hash[..8.min(hash.len())])).unwrap_or_default()
            );
        }
        HarnessEvent::ToolStart { call } => println!("\n[tool] {}", call.name),
        HarnessEvent::ToolResult { output, error, .. } => println!("\n[tool result] {}: {}", if error.unwrap_or(false) { "error" } else { "ok" }, output.chars().take(200).collect::<String>()),
        HarnessEvent::ToolApprovalRequired { risk, summary, .. } => eprintln!("\n[approval required: {risk}] {summary}"),
        HarnessEvent::Error { message } => eprintln!("\n[error] {message}"),
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_command(
    prompt: String,
    provider: Option<String>,
    model: Option<String>,
    endpoint: Option<String>,
    api_key_env: Option<String>,
    max_steps: usize,
    session_path: Option<PathBuf>,
    image_paths: Vec<PathBuf>,
    permission_mode: Option<String>,
    yes: bool,
    json_mode: bool,
    timeout: u64,
    retries: u32,
) -> Result<i32> {
    let config = default_config(&std::env::current_dir()?, &home_dir());
    let (_, mut options) = apply_settings(provider.as_deref(), model.clone(), endpoint.clone(), api_key_env.clone())?;
    options.timeout_ms = Some(timeout);
    options.max_retries = Some(retries);
    let crate_provider = crate::model::factory::create_provider(&options)?;

    // Run defaults to read-only like the TS CLI; `--yes` allows everything.
    let mode = if yes {
        PermissionMode::Full
    } else {
        permission_mode.as_deref().and_then(PermissionMode::from_str).unwrap_or(PermissionMode::ReadOnly)
    };
    let policy: SharedGatePolicy = Arc::new(GatePolicy::new(mode, &config.pi_config_dir, Vec::new()));
    // Approvals are resolved by policy only (non-interactive); prompts would deny.
    let approver_policy = policy.clone();
    let approve = move |request| {
        let policy = approver_policy.clone();
        Box::pin(async move {
            let audit = policy.authorize(&request);
            ApprovalResult { decision: audit.decision, remember: false }
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = ApprovalResult> + Send>>
    };

    let session = match session_path {
        Some(path) => SessionStore::open(&path)?,
        None => SessionStore::create(&config.cwd)?,
    };
    let initial_messages = session.messages()?;
    let images = load_images(&image_paths)?;

    // Compaction persistence is wired through the shared session cell; compaction
    // entry linkage (appendCompactionFromMessageIndex) is a later-port item.
    let session_cell = Arc::new(std::sync::Mutex::new(session));
    let mut agent = Agent::new(
        crate_provider,
        builtin_tools(&config.cwd),
        CompactionOptions::default(),
        None,
        max_steps,
        initial_messages,
        Some(Box::new(approve)),
    );

    let session_for_events = session_cell.clone();
    let json_flag = json_mode;
    let result = agent
        .run(&prompt, &mut |event| {
            {
                let mut store = session_for_events.lock().unwrap();
                match &event {
                    HarnessEvent::UserMessage { message } | HarnessEvent::AssistantMessage { message } => {
                        let _ = store.append_message(message);
                    }
                    HarnessEvent::ToolResult { call, output, .. } => {
                        let _ = store.append_message(&Message::tool(output.clone(), call.id.clone(), call.name.clone()));
                    }
                    HarnessEvent::Usage { prompt_tokens, completion_tokens, cached_tokens, prefix_hash, prefix_tokens, suffix_tokens, latency_ms } => {
                        let _ = store.append_usage("model", *prompt_tokens, *completion_tokens, *cached_tokens);
                        if let Some(prefix_hash) = prefix_hash {
                            let _ = store.append_cache(&options.provider.map(|id| id.as_str()).unwrap_or("?"), options.model.as_deref().unwrap_or("default"), prefix_hash, prefix_tokens.unwrap_or(0), suffix_tokens.unwrap_or(0), *prompt_tokens, *completion_tokens, *cached_tokens, *latency_ms);
                        }
                    }
                    _ => {}
                }
            }
            print_event(&event, json_flag);
        }, images)
        .await;

    result.map(|_| 0)
}

async fn models_command(provider: Option<String>, model: Option<String>, endpoint: Option<String>, api_key_env: Option<String>, timeout: Option<u64>, retries: u32) -> Result<i32> {
    let (_, options) = apply_settings(provider.as_deref(), model, endpoint, api_key_env)?;
    let options = ProviderOptions { timeout_ms: timeout, max_retries: Some(retries), ..options };
    let config = provider_config(&options)?;
    for info in discover_models(&config).await? {
        println!(
            "{}\t{}\tcontext={}\ttools={}\tvision={}",
            info.id,
            info.name,
            info.context_window.map(|window| window.to_string()).unwrap_or_else(|| "?".to_string()),
            info.supports_tools,
            info.supports_vision
        );
    }
    Ok(0)
}

pub async fn dispatch(cli: Cli) -> Result<i32> {
    let config = default_config(&std::env::current_dir()?, &home_dir());
    match cli.command {
        Some(Commands::Version) => {
            println!("{VERSION}");
            Ok(0)
        }
        Some(Commands::Run { prompt, provider, model, endpoint, api_key_env, max_steps, session, images, permission_mode, yes, json, timeout, retries }) => {
            run_command(prompt, provider, model, endpoint, api_key_env, max_steps, session, images, permission_mode, yes, json, timeout, retries).await
        }
        Some(Commands::Tui) => Err(anyhow::anyhow!("The TUI is not ported yet; use `run` (see docs/remaining-scope.md)")),
        Some(Commands::Models { provider, model, endpoint, api_key_env, timeout, retries }) => models_command(provider, model, endpoint, api_key_env, timeout, retries).await,
        Some(Commands::Settings) => {
            let (settings, sources) = load_settings(&config.cwd, &config.pi_config_dir);
            println!("Layers (highest priority first):");
            if sources.is_empty() {
                println!("  none");
            }
            for file in sources {
                println!("  {}", file.display());
            }
            let entries: Vec<String> = [
                settings.default_provider.clone().map(|value| format!("defaultProvider={value}")),
                settings.default_model.clone().map(|value| format!("defaultModel={value}")),
                settings.endpoint.clone().map(|value| format!("endpoint={value}")),
                settings.api_key_env.clone().map(|value| format!("apiKeyEnv={value}")),
                settings.context_window.map(|value| format!("contextWindow={value}")),
                settings.theme.clone().map(|value| format!("theme={value}")),
            ]
            .into_iter()
            .flatten()
            .collect();
            println!("Effective:");
            if entries.is_empty() {
                println!("  defaults only");
            }
            for entry in entries {
                println!("  {entry}");
            }
            println!("Project: {}", config.cwd.display());
            Ok(0)
        }
        Some(Commands::Trust { action, path }) => match action.as_str() {
            "list" => {
                for project in list_trusted_projects(&config.pi_config_dir) {
                    println!("{}\t{}\t{}", project.path, project.fingerprint, project.trusted_at);
                }
                Ok(0)
            }
            "grant" | "revoke" => {
                let target = path.clone().unwrap_or_else(|| config.cwd.clone());
                println!("{}", serde_json::to_string(&set_project_trust(&config.pi_config_dir, &target, action == "grant")?)?);
                Ok(0)
            }
            _ => {
                println!(
                    "{}\t{}\t{}",
                    config.cwd.display(),
                    if is_project_trusted(&config.cwd, &config.pi_config_dir) { "trusted" } else { "untrusted" },
                    project_fingerprint(&config.cwd)
                );
                Ok(0)
            }
        },
        None => Err(anyhow::anyhow!("Unknown command")),
        Some(Commands::Cache { session, json }) => {
            let store = SessionStore::open(&session)?;
            let entries = store.all_cache_entries();
            if json {
                println!("{}", serde_json::to_string_pretty(&entries)?);
            } else {
                for entry in entries {
                    let prompt_tokens = entry["usage"]["promptTokens"].as_u64().unwrap_or(0);
                    let cached_tokens = entry["usage"]["cachedTokens"].as_u64().unwrap_or(0);
                    let hit = if prompt_tokens > 0 { cached_tokens * 100 / prompt_tokens } else { 0 };
                    let latency = entry["latencyMs"].as_str().map(str::to_string).unwrap_or_else(|| entry["latencyMs"].as_u64().map(|ms| ms.to_string()).unwrap_or_else(|| "?".to_string()));
                    println!("{}\t{}/{}\t{}\tprompt={prompt_tokens}\tcached={cached_tokens}\thit={hit}%\tlatency={latency}ms",
                        entry["timestamp"].as_str().unwrap_or("?"),
                        entry["provider"].as_str().unwrap_or("?"),
                        entry["model"].as_str().unwrap_or("?"),
                        entry["prefixHash"].as_str().unwrap_or("?").chars().take(12).collect::<String>());
                }
            }
            Ok(0)
        }
    }
}
