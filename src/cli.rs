//! CLI, ported from `src/cli.ts` (run, models, cache, trust, settings, version).

use crate::config::{default_config, home_dir, is_project_trusted, list_trusted_projects, load_settings, project_fingerprint, set_project_trust, HarnessSettings};
use crate::core::agent::{Agent, CompactionOptions};
use crate::model::discovery::discover_models;
use crate::model::factory::{provider_config, ProviderId, ProviderOptions};
use crate::security::permissions::{ApprovalResult, GatePolicy, PermissionMode, SharedGatePolicy};
use crate::mcp::client::{load_mcp_servers, add_mcp_server, remove_mcp_server, save_mcp_servers, sanitize_mcp_name, McpManager, McpOAuthConfig, McpServerConfig, McpTool};
use crate::sessions::store::SessionStore;
use crate::tools::builtin_tools;
use crate::types::{HarnessEvent, ImageAttachment, Message};
use anyhow::{anyhow, bail, Result};
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
        /// Connect a configured MCP server; may be repeated.
        #[arg(long = "mcp")]
        mcp_servers: Vec<String>,
        /// Use a provider registered by an extension.
        #[arg(long = "extension-provider")]
        extension_provider: Option<String>,
        /// Reasoning amount: off|low|medium|high (default from defaultThinkingLevel).
        #[arg(long)]
        reasoning: Option<String>,
        #[arg(long)]
        allow_tool: Vec<String>,
        #[arg(long)]
        allow_risk: Vec<String>,
    },
    Resources,
    Packages {
        action: Option<String>,
        #[arg(default_value = None)]
        source: Option<String>,
        /// Write settings at project scope (.pi/settings.json); requires trust
        #[arg(long)]
        project: bool,
    },
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
    Mcp {
        #[arg(default_value = "list")]
        action: String,
        #[arg(default_value = None)]
        name: Option<String>,
        /// Remaining flags (--project/--command/--args/--url/--oauth-*)
        #[arg(allow_hyphen_values = true, trailing_var_arg = true)]
        extra: Vec<String>,
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
    cli_reasoning: Option<&str>,
) -> Result<(ProviderId, ProviderOptions)> {
    let config = default_config(&std::env::current_dir()?, &home_dir());
    let (settings, _) = load_settings(&config.cwd, &config.pi_config_dir);
    let HarnessSettings { default_provider, default_model, endpoint, api_key_env, context_window, theme: _, default_thinking_level } = settings;
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
    // Reasoning precedence: flag > env (HARNESS_REASONING) > settings.defaultThinkingLevel > off.
    let env_reasoning = std::env::var("HARNESS_REASONING").ok().filter(|value| !value.is_empty());
    let reasoning_name = cli_reasoning.map(str::to_string).or(env_reasoning).or(default_thinking_level);
    let reasoning = reasoning_name.as_deref().and_then(crate::types::ReasoningLevel::from_str);
    Ok((
        provider,
        ProviderOptions { provider: Some(provider), model, endpoint, api_key: None, api_key_env, session_id: None, timeout_ms: None, max_retries: None, context_window, headers: Vec::new(), reasoning },
    ))
}

pub fn load_images_public(paths: &[PathBuf]) -> Result<Vec<ImageAttachment>> {
    load_images(paths)
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

/// Select an extension-registered provider or fall back to the standard one.
fn resolve_extension_provider(
    selected: Option<&str>,
    extension_host: Option<&Arc<crate::extensions::ExtensionHost>>,
    api_key: Option<String>,
) -> anyhow::Result<Arc<dyn crate::model::provider::Provider>> {
    let Some(extension_host) = extension_host else {
        bail!("Extension host is not running; load an extensions/ directory first");
    };
    let Some(selected) = selected else {
        bail!("No --extension-provider selected");
    };
    let provider = extension_host
        .provider_list()
        .into_iter()
        .find(|provider| provider.id == selected)
        .ok_or_else(|| anyhow!("Unknown extension provider: {selected}"))?;
    let api_key = api_key.or_else(|| {
        provider.api_key_env.as_ref().and_then(|env| std::env::var(env).ok().filter(|value| !value.is_empty()))
    });
    crate::model::factory::create_provider(&ProviderOptions {
        provider: Some(ProviderId::Custom),
        model: Some(provider.model.clone()),
        endpoint: Some(provider.base_url.clone()),
        api_key,
        headers: provider.headers.clone(),
        ..ProviderOptions::new()
    })
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
    mcp_servers: Vec<String>,
    extension_provider: Option<String>,
    reasoning: Option<String>,
    allow_tool: Vec<String>,
    allow_risk: Vec<String>,
) -> Result<i32> {
    let config = default_config(&std::env::current_dir()?, &home_dir());
    let (_, mut options) = apply_settings(provider.as_deref(), model.clone(), endpoint.clone(), api_key_env.clone(), reasoning.as_deref())?;
    options.timeout_ms = Some(timeout);
    options.max_retries = Some(retries);
    let crate_provider = crate::model::factory::create_provider(&options)?;
    let mut crate_provider = crate_provider;

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

    let session_cell = Arc::new(std::sync::Mutex::new(session));
    let tokens_before = serde_json::to_string(&initial_messages).map(|text| text.len().div_ceil(4)).unwrap_or(0);
    let compaction_session = session_cell.clone();
    // Extension host: trusted project `extensions/` directory.
    let extension_paths = crate::extensions::discover_extensions(&config.cwd, is_project_trusted(&config.cwd, &config.pi_config_dir));
    let mut tools: Vec<Arc<dyn crate::tools::Tool>> = builtin_tools(&config.cwd);
    let mut extension_tools: Option<(Arc<crate::extensions::ExtensionHost>, Arc<dyn crate::extensions::AgentObserver>)> = None;
    let mut extension_host: Option<Arc<crate::extensions::ExtensionHost>> = None;
    if !extension_paths.is_empty() {
        let host = crate::extensions::ExtensionHost::new(extension_paths, &config.cwd);
        match host.start().await {
            Ok(_) => {
                let observer: Arc<dyn crate::extensions::AgentObserver> = Arc::new(crate::extensions::HostObserver { host: host.clone() });
                extension_host = Some(host.clone());
                for name in host.tool_names() {
                    if let Ok(wrapper) = host.tool_wrapper(&name) {
                        tools.push(wrapper);
                    }
                }
                extension_tools = Some((host, observer));
            }
            Err(error) => eprintln!("[extension] {error}"),
        }
    }
    let extension_provider_selection = extension_provider.clone();
    let _ = extension_provider_selection;
    let mcp_manager = if !mcp_servers.is_empty() { Some(connect_mcp_manager(&mcp_servers, &policy).await?) } else { None };
    if let Some(manager) = &mcp_manager {
        for name in manager.tool_names() {
            if let Some(entry) = manager.get_tool(&name) {
                tools.push(Arc::new(McpTool { entry }));
            }
        }
    }
    if extension_provider.is_some() {
        crate_provider = resolve_extension_provider(extension_provider.as_deref(), extension_host.as_ref(), None)?;
    }
    let mut agent = Agent::new(
        crate_provider,
        tools,
        CompactionOptions {
            on_compaction: Some(Box::new(move |summary, index| {
                let mut store = compaction_session.lock().unwrap();
                let _ = store.append_compaction_from_message_index(summary, index, tokens_before);
            })),
            ..CompactionOptions::default()
        },
        None,
        max_steps,
        initial_messages,
        Some(Box::new(approve)),
        None,
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


impl McpServerConfig {
    fn describe(&self) -> String {
        self.command.clone().or_else(|| self.url.clone()).unwrap_or_default()
    }

    fn enabled_label(&self) -> &'static str {
        if self.enabled == Some(false) { "disabled" } else { "enabled" }
    }
}

async fn mcp_command(args: &[String]) -> Result<i32> {
    let config = default_config(&std::env::current_dir()?, &home_dir());
    let project = args.iter().any(|arg| arg.as_str() == "--project");
    if project && !is_project_trusted(&config.cwd, &config.pi_config_dir) {
        bail!("Project is not trusted; run `hl-harness trust` first");
    }
    let action = args.first().map(String::as_str).unwrap_or("list");
    let rest: Vec<String> = args.iter().skip(1).cloned().collect();
    match action {
        "list" => {
            let servers = load_mcp_servers(&config.cwd, &config.pi_config_dir, is_project_trusted(&config.cwd, &config.pi_config_dir))?;
            for (name, server) in servers {
                println!("{name}\t{}\t{}", server.enabled_label(), server.describe());
            }
            Ok(0)
        }
        "add" => {
            let Some(name) = rest.first().cloned() else { bail!("mcp add requires a name") };
            let flag = |flag: &str| rest.iter().position(|arg| arg.as_str() == flag).and_then(|index| rest.get(index + 1)).cloned();
            let oauth_value2 = |flag_value: &str| rest.iter().position(|arg| arg.as_str() == flag_value).and_then(|index| rest.get(index + 1)).cloned();
    let oauth_value = oauth_value2;
            let oauth_client_id = oauth_value2("--oauth-client-id");
            let oauth_secret_env = oauth_value("--oauth-client-secret-env");
            let oauth_redirect_uri = oauth_value("--oauth-redirect-uri");
            let oauth_scope = oauth_value("--oauth-scope");
            let oauth_secret = oauth_secret_env.clone().and_then(|env| std::env::var(env).ok());
            let oauth = oauth_client_id.clone().map(|client_id| McpOAuthConfig {
                client_id,
                client_secret: oauth_secret,
                redirect_uri: oauth_redirect_uri,
                scope: oauth_scope,
                token_file: None,
            });
            let command_value = oauth_value("--command");
            let url_value = oauth_value("--url");
            let command_index = rest.iter().position(|arg| arg.as_str() == "--command");
            let url_index = rest.iter().position(|arg| arg.as_str() == "--url");
            let start = command_index.map(|index| index + 2).unwrap_or(0);
            let stop = url_index.unwrap_or(rest.len());
            let raw_args: Vec<String> = if command_index.is_some() && start <= stop {
                rest[start..stop].iter().filter(|value| !matches!(value.as_str(), "--command" | "--url" | "--args")).cloned().collect::<Vec<String>>()
            } else {
                Vec::new()
            };
            let command_args = raw_args.iter().position(|arg| arg == "--args").map(|index| raw_args[index + 1..].to_vec()).unwrap_or_else(|| raw_args.clone());
            let server = McpServerConfig {
                command: command_value,
                args: if !command_args.is_empty() { Some(command_args) } else { None },
                url: url_value,
                oauth,
                env: None,
                headers: None,
                enabled: None,
            };
            let has_command = command_index.is_some();
            let has_url = url_index.is_some();
            if oauth_client_id.is_some() && !has_command && !has_url {
                bail!("MCP OAuth requires --url");
            }
            println!("{}", add_mcp_server(&config.cwd, &config.pi_config_dir, project, &name, server)?);
            Ok(0)
        }
        "remove" => {
            let Some(name) = rest.first().cloned() else { bail!("mcp remove requires a name") };
            println!("{}", if remove_mcp_server(&config.cwd, &config.pi_config_dir, project, &name)? { "removed" } else { "not found" });
            Ok(0)
        }
        other => bail!("Unknown mcp action: {other}"),
    }
}

/// Non-interactive MCP interactions decline server requests (matching the TS
/// behavior when no handlers are configured).
struct DecliningInteractions;

impl crate::mcp::client::McpInteractions for DecliningInteractions {
    fn elicit(&self, _request: serde_json::Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<serde_json::Value>> + Send>> {
        Box::pin(async { Ok(serde_json::json!({"action": "decline"})) })
    }
    fn sample(&self, _request: serde_json::Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<serde_json::Value>> + Send>> {
        Box::pin(async { Err(anyhow::anyhow!("sampling is declined in non-interactive run mode")) })
    }
}

async fn mcp_connect_selected(manager: &McpManager, names: &[String], policy: &GatePolicy) -> Result<()> {
    let approve: std::sync::Arc<crate::mcp::client::ApproveFn> = std::sync::Arc::new({
        let policy = policy.clone();
        move |request| {
            let policy = policy.clone();
            Box::pin(async move {
                let audit = policy.authorize(&request);
                ApprovalResult { decision: audit.decision, remember: false }
            })
        }
    });
    for name in names {
        manager.connect(name, Some(approve.as_ref())).await?;
    }
    Ok(())
}

pub async fn connect_mcp_manager(names: &[String], policy: &GatePolicy) -> Result<McpManager> {
    let config = default_config(&std::env::current_dir()?, &home_dir());
    let servers = load_mcp_servers(&config.cwd, &config.pi_config_dir, is_project_trusted(&config.cwd, &config.pi_config_dir))?;
    let manager = McpManager::new(&config.cwd, servers, None);
    mcp_connect_selected(&manager, names, policy).await?;
    Ok(manager)
}
async fn resources_command() -> Result<i32> {
    let config = default_config(&std::env::current_dir()?, &home_dir());
    let project_trusted = is_project_trusted(&config.cwd, &config.pi_config_dir);
    let options = crate::resources::ResourceLoadOptions {
        cwd: config.cwd.clone(),
        personal_dir: config.pi_config_dir.clone(),
        include_project: true,
        project_trusted,
        package_roots: crate::resources::packages::resolve_package_roots(
            &crate::resources::packages::package_sources_from_settings(&config.pi_config_dir, Some(&config.cwd)),
            &crate::resources::packages::ResolveOptions { cwd: &config.cwd, personal_dir: &config.pi_config_dir, install: false },
            &|message| eprintln!("[packages] {message}"),
        ),
    };
    let resources = crate::resources::load_resources(&options)?;
    println!("Skill resources: {}", resources.registry.list(Some(crate::resources::ResourceKind::Skill)).len());
    println!("Prompt resources: {}", resources.registry.list(Some(crate::resources::ResourceKind::Prompt)).len());
    println!("Theme resources: {}", resources.registry.list(Some(crate::resources::ResourceKind::Theme)).len());
    println!("Extension resources: {}", resources.registry.list(Some(crate::resources::ResourceKind::Extension)).len());
    println!("Context files: {}", resources.context_files.len());
    Ok(0)
}

async fn packages_command(action: Option<String>, source: Option<String>, project: bool) -> Result<i32> {
    let config = default_config(&std::env::current_dir()?, &home_dir());
    let project_trusted = is_project_trusted(&config.cwd, &config.pi_config_dir);
    let managed = crate::resources::packages::list_managed_packages(&config.pi_config_dir, Some(&config.cwd));
    match action.as_deref() {
        None | Some("list") => {
            let package_roots = crate::resources::packages::resolve_package_roots(
                &crate::resources::packages::package_sources_from_settings(&config.pi_config_dir, Some(&config.cwd)),
                &crate::resources::packages::ResolveOptions { cwd: &config.cwd, personal_dir: &config.pi_config_dir, install: false },
                &|message| eprintln!("[packages] {message}"),
            );
            let resources = crate::resources::load_resources(&crate::resources::ResourceLoadOptions {
                cwd: config.cwd.clone(),
                personal_dir: config.pi_config_dir.clone(),
                include_project: true,
                project_trusted,
                package_roots,
            })?;
            println!("Package resources: {}", resources.registry.len());
            for kind in [crate::resources::ResourceKind::Extension, crate::resources::ResourceKind::Skill, crate::resources::ResourceKind::Prompt, crate::resources::ResourceKind::Theme] {
                println!("{}s: {}", kind.as_str(), resources.registry.list(Some(kind)).len());
            }
            for item in managed {
                println!("{}\t{}\t{}", if item.package.enabled { "enabled" } else { "disabled" }, item.package.source, item.file.display());
            }
            Ok(0)
        }
        Some("install") => {
            for source in crate::resources::packages::package_sources_from_settings(&config.pi_config_dir, Some(&config.cwd)) {
                let resolution = crate::resources::packages::install_package(&source, &crate::resources::packages::ResolveOptions { cwd: &config.cwd, personal_dir: &config.pi_config_dir, install: false })?;
                println!("{}\t{}", resolution.source, resolution.root.display());
            }
            Ok(0)
        }
        Some("update") | Some("enable") | Some("disable") | Some("remove") => {
            let Some(source) = source.clone() else { bail!("packages {} requires a source", action.unwrap()) };
            let project_scope = if project {
                if !project_trusted {
                    bail!("Project is not trusted; run `hl-harness trust` first");
                }
                true
            } else {
                false
            };
            let item = managed.iter().find(|candidate| candidate.package.source == source).cloned().ok_or_else(|| anyhow!("Package is not configured: {source}"))?;
            let file = item.file.clone();
            match action.as_deref() {
                Some("update") => {
                    let resolution = crate::resources::packages::update_package(&source, &crate::resources::packages::ResolveOptions { cwd: &config.cwd, personal_dir: &config.pi_config_dir, install: false })?;
                    println!("{}\t{}", resolution.source, resolution.root.display());
                }
                Some("enable") => crate::resources::packages::set_package_enabled(&file, &source, true)?,
                Some("disable") => crate::resources::packages::set_package_enabled(&file, &source, false)?,
                Some("remove") => {
                    let removed = crate::resources::packages::remove_managed_package(&file, &source)?;
                    if !removed {
                        return Ok(0);
                    }
                }
                _ => {}
            }
            let _ = project_scope;
            println!("{} {}", action.unwrap(), source);
            Ok(0)
        }
        Some(other) => bail!("Unknown packages action: {other}"),
    }
}

async fn models_command(provider: Option<String>, model: Option<String>, endpoint: Option<String>, api_key_env: Option<String>, timeout: Option<u64>, retries: u32) -> Result<i32> {
    let (_, options) = apply_settings(provider.as_deref(), model, endpoint, api_key_env, None)?;
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
        Some(Commands::Run { prompt, provider, model, endpoint, api_key_env, max_steps, session, images, permission_mode, yes, json, timeout, retries, mcp_servers, extension_provider, reasoning, allow_tool, allow_risk }) => {
            run_command(prompt, provider, model, endpoint, api_key_env, max_steps, session, images, permission_mode, yes, json, timeout, retries, mcp_servers, extension_provider.clone(), reasoning.clone(), allow_tool, allow_risk).await
        }
        Some(Commands::Resources) => resources_command().await,
        Some(Commands::Packages { action, source, project }) => packages_command(action, source, project).await,
        Some(Commands::Tui) => crate::cli_tui::run_tui().await,
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
        Some(Commands::Mcp { action, name, extra }) => {
            let mut all_args = vec![action.clone()];
            if let Some(name) = name {
                all_args.push(name.clone());
            }
            all_args.extend(extra.clone());
            mcp_command(&all_args).await
        }
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
