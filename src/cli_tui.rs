//! Interactive TUI command, ported from the TS `runTui()` in `src/cli.ts`.
//! Commands wired to the ported subsystems: /trust, /cache, /diff, /conflicts,
//! /compact, /tree, /branch, /attach, /mcp(/auth); /auth, /template and /skill
//! surface guidance until the extension-host phase lands.

use crate::config::{default_config, home_dir, is_project_trusted, load_settings, set_project_trust};
use crate::core::agent::{Agent, CompactionOptions};
use crate::model::factory::{create_provider, ProviderId, ProviderOptions};
use crate::mcp::client::{load_mcp_servers, McpManager, McpTool};
use crate::security::permissions::{ApprovalResult, GatePolicy, PermissionMode, SharedGatePolicy};
use crate::sessions::store::SessionStore;
use crate::tools::{builtin_tools, Tool};
use crate::types::{HarnessEvent, ImageAttachment, Message};
use anyhow::{anyhow, bail, Result};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use crate::tui::terminal::{Intent, TerminalUi};
use crate::tui::{render::CacheHistoryEntry, workspace};


async fn session_messages(session: &Arc<tokio::sync::Mutex<SessionStore>>) -> Vec<Message> {
    session.lock().await.messages().unwrap_or_default()
}

fn mcp_approver(policy: &SharedGatePolicy) -> Box<crate::mcp::client::ApproveFn> {
    let policy = policy.clone();
    Box::new(move |request| {
        let policy = policy.clone();
        Box::pin(async move {
            let audit = policy.authorize(&request);
            ApprovalResult { decision: audit.decision, remember: false }
        })
    })
}

fn tool_count(manager: &McpManager, name: &str) -> usize {
    manager.tool_names().iter().filter(|tool| tool.starts_with(&format!("mcp__{name}__"))).count()
}

async fn mcp_connect(manager: &McpManager, name: &str, policy: &SharedGatePolicy) -> Result<usize> {
    let approve: Arc<crate::mcp::client::ApproveFn> = Arc::new({
        let policy = policy.clone();
        move |request| {
            let policy = policy.clone();
            Box::pin(async move {
                let audit = policy.authorize(&request);
                ApprovalResult { decision: audit.decision, remember: false }
            })
        }
    });
    manager.connect(name, Some(approve.as_ref())).await?;
    Ok(tool_count(manager, name))
}

/// Placeholder for the pending-images store; multi-image submission lands with
/// the image-attachment phase of the TUI port.
static PENDING_IMAGE_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

static RUN_COUNT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

const QUOTES: [&str; 5] = [
    "ship small, ship often",
    "the diff is the truth",
    "every bug was once a feature request",
    "read the error message first",
    "make something delightful",
];

/// One-line git summary for the `git` widget: branch · changes · ahead/behind.
fn git_widget_value(cwd: &std::path::Path) -> String {
    let output = std::process::Command::new("git")
        .current_dir(cwd)
        .args(["status", "--porcelain", "--branch"])
        .output();
    let Ok(output) = output else { return "git unavailable".to_string() };
    if !output.status.success() {
        return "not a git repo".to_string();
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut branch = String::new();
    let mut tracking = String::new();
    let mut changed = 0usize;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("## ") {
            let (head, tail) = rest.split_once("...").unwrap_or((rest, ""));
            branch = head.to_string();
            if let Some((_, info)) = tail.split_once('[') {
                tracking = info.trim_end_matches(']').to_string();
            }
        } else if !line.trim().is_empty() {
            changed += 1;
        }
    }
    if branch.is_empty() {
        return "not a git repo".to_string();
    }
    let mut value = branch;
    if changed > 0 {
        value.push_str(&format!(" · {changed} changed"));
    } else {
        value.push_str(" · clean");
    }
    if !tracking.is_empty() {
        value.push_str(&format!(" · {tracking}"));
    }
    value
}

fn refresh_git_widget(ui: &Arc<TerminalUi>) {
    if let Ok(cwd) = std::env::current_dir() {
        ui.set_widget_value("git", &git_widget_value(&cwd));
    }
}

fn next_quote() -> &'static str {
    QUOTES[RUN_COUNT.fetch_add(1, Ordering::SeqCst) % QUOTES.len()]
}

fn format_token_count(tokens: u64) -> String {
    if tokens >= 1_000_000 {
        format!("{:.1}m", tokens as f64 / 1_000_000.0)
    } else if tokens >= 1_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else {
        tokens.to_string()
    }
}

/// Context widget value: current window size (last prompt tokens) against the
/// model context window when known.
fn context_widget_value(prompt_tokens: u64, window: Option<u64>) -> String {
    match window {
        Some(window) if window > 0 => {
            let percent = (prompt_tokens * 100 / window).min(100);
            format!("{} / {} tok · {}%", format_token_count(prompt_tokens), format_token_count(window), percent)
        }
        _ => format!("{} tok", format_token_count(prompt_tokens)),
    }
}

#[allow(clippy::too_many_lines)]
async fn handle_tui_submit(
    ui: &Arc<TerminalUi>,
    submitted: &str,
    session_cell: &Arc<tokio::sync::Mutex<SessionStore>>,
    options_cell: &Arc<tokio::sync::Mutex<Option<ProviderOptions>>>,
    agent_cell: &Arc<tokio::sync::Mutex<Option<Agent>>>,
    policy: &SharedGatePolicy,
    max_steps: usize,
    mcp_manager: &Arc<tokio::sync::Mutex<Option<Arc<McpManager>>>>,
    extension_host: &Arc<tokio::sync::Mutex<Option<Arc<crate::extensions::ExtensionHost>>>>,
    reasoning_cell: &Arc<tokio::sync::Mutex<crate::types::ReasoningLevel>>,
    cancel: &Arc<AtomicBool>,
    busy: &Arc<AtomicBool>,
) -> Result<i32> {
    let submitted = submitted.trim().to_string();
    if submitted.is_empty() {
        return Ok(0);
    }
    if submitted == "/trust" {
        ui.push_transcript("system: type /trust yes to trust this project, or /trust no to keep it untrusted".into());
        ui.set_status("Trust confirmation required");
        return Ok(0);
    }
    if submitted == "/trust yes" || submitted == "/trust no" {
        let trusted = submitted.ends_with(" yes");
        let config = default_config(&std::env::current_dir()?, &home_dir());
        let record = set_project_trust(&config.pi_config_dir, &config.cwd, trusted)?;
        if trusted {
            ui.push_transcript(format!("system: trusted {} ({}); restart TUI to load project resources", record.path, record.fingerprint));
            ui.set_status("Project trusted; restart required");
        } else {
            ui.push_transcript(format!("system: revoked trust for {} ({}); restart TUI to unload project resources", record.path, record.fingerprint));
            ui.set_status("Project trust revoked; restart required");
        }
        return Ok(0);
    }
    if submitted == "/conflicts close" {
        ui.set_conflicts(None);
        ui.push_transcript("system: Conflict view closed".into());
        return Ok(0);
    }
    if submitted == "/conflicts" {
        let report = if ui.has_conflicts() { ui.conflict_report() } else { "No conflicts recorded; run git_conflicts.".to_string() };
        ui.set_conflicts(Some(report));
        ui.push_transcript("system: Conflict view updated".into());
        return Ok(0);
    }
    if let Some(action) = submitted.strip_prefix("/cache") {
        let action = action.trim().to_string();
        if action.is_empty() {
            let entries = session_cell.lock().await.all_cache_entries();
            let mapped = entries
                .iter()
                .map(|entry| CacheHistoryEntry {
                    prefix_hash: entry["prefixHash"].as_str().unwrap_or_default().to_string(),
                    prompt_tokens: entry["usage"]["promptTokens"].as_u64().unwrap_or(0),
                    cached_tokens: entry["usage"]["cachedTokens"].as_u64().unwrap_or(0),
                    latency_ms: entry["latencyMs"].as_u64(),
                })
                .collect();
            ui.set_cache_history(mapped);
        }
        ui.push_transcript(format!("system: {}", ui.cache_command(&action)));
        ui.set_status("Cache monitor updated");
        return Ok(0);
    }
    if let Some(action) = submitted.strip_prefix("/diff") {
        let action = action.trim().to_string();
        ui.push_transcript(format!("system: {}", ui.diff_command(&action)));
        ui.set_status("Diff navigation updated");
        return Ok(0);
    }
    if let Some(rest) = submitted.strip_prefix("/mcp auth") {
        let parts: Vec<&str> = rest.trim().splitn(2, ' ').collect();
        if parts.len() != 2 || parts[0].is_empty() {
            ui.push_transcript("error: /mcp auth requires <name> <code>".into());
            return Ok(0);
        }
        let name = parts[0].to_string();
        let code = parts[1].to_string();
        let manager = mcp_manager.lock().await.as_ref().cloned();
        let Some(manager) = manager else {
            ui.push_transcript("system: MCP is unavailable".into());
            return Ok(0);
        };
        match manager.finish_auth(&name, &code).await {
            Ok(_) => {
                let _ = mcp_connect(&manager, &name, policy).await;
                ui.push_transcript(format!("system: authenticated and connected MCP {name}"));
            }
            Err(error) => ui.push_transcript(format!("error: {error}")),
        }
        ui.set_status("MCP authentication updated");
        return Ok(0);
    }
    if let Some(name_stripped) = submitted.strip_prefix("/mcp") {
        let name = name_stripped.trim().to_string();
        let config = default_config(&std::env::current_dir()?, &home_dir());
        let servers = load_mcp_servers(&config.cwd, &config.pi_config_dir, is_project_trusted(&config.cwd, &config.pi_config_dir))?;
        if name.is_empty() {
            let preview = McpManager::new(&config.cwd, servers, Some(Arc::new(crate::tui_interactions::TuiInteractions)));
            ui.push_transcript(format!("mcp servers: {}", preview.names().join(", ")));
            return Ok(0);
        }
        if !servers.contains_key(&name) {
            ui.push_transcript(format!("system: server {name} is not configured"));
            return Ok(0);
        }
        let missing_manager = mcp_manager.lock().await.is_none();
        if missing_manager {
            *mcp_manager.lock().await = Some(Arc::new(McpManager::new(&config.cwd, servers, None)));
        }
        let connect_result = {
            let manager = mcp_manager.lock().await.as_ref().cloned();
            let Some(manager) = manager else { bail!("MCP manager unavailable") };
            mcp_connect(&manager, &name, policy).await
        };
        match connect_result {
            Ok(count) => {
                ui.push_transcript(format!("system: connected MCP {name} ({count} tools)"));
                ui.set_status("MCP updated");
            }
            Err(error) => {
                ui.push_transcript(format!("error: {error}"));
                ui.set_status("MCP connect failed");
            }
        }
        return Ok(0);
    }
    if submitted == "/tree" {
        let entries: Vec<Value> = { let guard = session_cell.lock().await; guard.all_entries().iter().cloned().collect() };
        let mut lines: Vec<String> = vec!["id\ttype\tparent".to_string()];
        for entry in entries {
            lines.push(format!("{}\t{}\tparent={}", entry["id"].as_str().unwrap_or("?"), entry["type"].as_str().unwrap_or("?"), entry["parentId"].as_str().map(str::to_string).unwrap_or_else(|| "root".into())));
        }
        ui.append_transcript(lines);
        ui.set_status("Session tree");
        return Ok(0);
    }
    if let Some(id_stripped) = submitted.strip_prefix("/branch") {
        let id = id_stripped.trim().to_string();
        if id.is_empty() {
            ui.push_transcript("error: /branch requires an entry id".into());
            return Ok(0);
        }
        match session_cell.lock().await.set_leaf(Some(&id)) {
            Ok(_) => {
                // The next submission rebuilds the agent from the projected branch.
                *agent_cell.lock().await = None;
                ui.push_transcript(format!("system: switched to {id}"));
                ui.set_status("Branch selected");
            }
            Err(error) => {
                ui.push_transcript(format!("error: {error}"));
                ui.set_status("Branch selection failed");
            }
        }
        return Ok(0);
    }
    if let Some(path_stripped) = submitted.strip_prefix("/attach") {
        let path = path_stripped.trim().to_string();
        if path.is_empty() {
            ui.push_transcript("error: /attach requires an image path".into());
            return Ok(0);
        }
        match crate::cli::load_images_public(&[std::path::PathBuf::from(&path)]) {
            Ok(images) => {
                PENDING_IMAGE_COUNT.fetch_add(images.len(), Ordering::SeqCst);
                ui.push_transcript(format!("system: attached {path}"));
                ui.set_status(&format!("{} image{} attached", PENDING_IMAGE_COUNT.load(Ordering::SeqCst), if images.len() == 1 { "" } else { "s" }));
            }
            Err(error) => {
                ui.push_transcript(format!("error: {error}"));
                ui.set_status("Attach failed");
            }
        }
        return Ok(0);
    }
    // /model [provider] <model>: rebuild the provider + agent with a new model.
    if let Some(rest) = submitted.strip_prefix("/model").map(|value| value.trim().to_string()) {
        if rest.is_empty() {
            let current = options_cell.lock().await.clone();
            let label = match current {
                Some(options) => format!("model: {} · provider: {}", options.model.clone().unwrap_or_else(|| "settings default".into()), options.provider.unwrap_or(ProviderId::OpenRouter).as_str()),
                None => "model: settings default".to_string(),
            };
            ui.push_transcript(format!("system: {label} (usage: /model [provider] <model-id>)"));
            return Ok(0);
        }
        let words: Vec<&str> = rest.split_whitespace().collect();
        let (provider_word, model_word) = match words.len() {
            1 => (None, words[0]),
            _ => (Some(words[0]), words[1]),
        };
        let mut options = options_cell.lock().await.clone().unwrap_or_default();
        if let Some(provider_word) = provider_word {
            match ProviderId::from_str(provider_word) {
                Some(provider) => options.provider = Some(provider),
                None => {
                    ui.push_transcript(format!("error: unknown provider {provider_word}"));
                    return Ok(0);
                }
            }
        }
        options.model = Some(model_word.to_string());
        match create_provider(&options) {
            Ok(provider) => {
                *options_cell.lock().await = Some(options.clone());
                let messages = agent_cell.lock().await.as_ref().map(|agent| agent.messages().to_vec()).unwrap_or_default();
                let mut agent = Agent::new(provider, builtin_tools(&config_path()), CompactionOptions::default(), None, max_steps, messages, Some(mcp_approver(policy)), None);
                agent.set_reasoning(*reasoning_cell.lock().await);
                *agent_cell.lock().await = Some(agent);
                ui.set_widget_value("model", &format!("{} · {}", options.provider.unwrap_or(ProviderId::OpenRouter).as_str(), model_word));
                ui.push_transcript(format!("system: model switched to {model_word}"));
                ui.set_status("Model switched");
            }
            Err(error) => {
                ui.push_transcript(format!("error: {error}"));
                ui.set_status("Model switch failed");
            }
        }
        return Ok(0);
    }
    // /reasoning <off|low|medium|high>
    if let Some(rest) = submitted.strip_prefix("/reasoning").map(|value| value.trim().to_string()) {
        if rest.is_empty() {
            ui.push_transcript(format!("system: reasoning is {}", reasoning_cell.lock().await.as_str()));
            ui.push_transcript("system: usage: /reasoning off|low|medium|high".to_string());
            return Ok(0);
        }
        match crate::types::ReasoningLevel::from_str(&rest) {
            Some(level) => {
                *reasoning_cell.lock().await = level;
                if let Some(agent) = agent_cell.lock().await.as_mut() {
                    agent.set_reasoning(level);
                }
                ui.set_widget_value("model", &format!("{}/reasoning {}", current_model_label(options_cell).await, level.as_str()));
                ui.push_transcript(format!("system: reasoning set to {}", level.as_str()));
                ui.set_status("Reasoning updated");
            }
            None => {
                ui.push_transcript(format!("error: unknown reasoning level {rest}"));
            }
        }
        return Ok(0);
    }
    if submitted == "/compact" {
        let mut agent = agent_cell.lock().await;
        match agent.as_mut() {
            Some(agent) => match agent.compact(usize::MAX).await {
                Ok(true) => {
                    ui.push_transcript("system: context compacted".into());
                    ui.set_status("Compaction complete");
                }
                Ok(false) => {
                    ui.push_transcript("system: nothing to compact".into());
                    ui.set_status("Compaction complete");
                }
                Err(error) => {
                    ui.push_transcript(format!("error: {error}"));
                    ui.set_status("Compaction failed");
                }
            },
            None => ui.push_transcript("system: provider is not configured".into()),
        }
        return Ok(0);
    }
    if submitted == "/auth" || submitted == "/login" {
        ui.push_transcript("system: /auth is not ported yet; set OPENCODE_API_KEY, OPENROUTER_API_KEY, ANTHROPIC_API_KEY, or HARNESS_API_KEY in the environment".into());
        ui.set_status("API keys: set via environment");
        return Ok(0);
    }
    // Extension commands: /<name> dispatches to the extension host when the
    // command is registered there.
    if submitted.starts_with('/') {
        if let Some(host) = extension_host.lock().await.as_ref() {
            let body = submitted.trim_start_matches('/');
            let (name, text) = match body.split_once(' ') {
                Some((name, text)) => (name, text),
                None => (body, ""),
            };
            if host.command_list().iter().any(|command| command.name == name) {
                match host.call_command(name, text).await {
                    Ok(output) => {
                        if !output.is_empty() {
                            ui.push_transcript(output);
                        }
                        ui.set_status("Extension command complete");
                    }
                    Err(error) => {
                        ui.push_transcript(format!("error: {error}"));
                        ui.set_status("Extension command failed");
                    }
                }
                return Ok(0);
            }
        }
        ui.push_transcript("system: unknown command".into());
        ui.set_status("Unknown command");
        return Ok(0);
    }

    // Prompt: run the agent loop.
    if busy.load(Ordering::SeqCst) {
        ui.push_transcript("system: a request is already running; press Esc to cancel first".into());
        return Ok(0);
    }
    cancel.store(false, Ordering::SeqCst);
    busy.store(true, Ordering::SeqCst);
    let ui_task = ui.clone();
    ui_task.set_quote(next_quote().to_string());
    ui_task.set_working(true, ui_task.clone());
    let session_task = session_cell.clone();
    let agent_task = agent_cell.clone();
    let options_task = options_cell.clone();
    let policy_task = policy.clone();
    let cancel_task = cancel.clone();
    let busy_task = busy.clone();
    let mcp_task = mcp_manager.clone();
    let extension_task = extension_host.clone();
    let reasoning_cell_task = reasoning_cell.clone();
    let max_steps_task = max_steps;
    tokio::spawn(async move {
        ui_task.push_transcript(format!("> {submitted}"));
        ui_task.set_status("Thinking…");
        let run = tui_run_agent(
            &ui_task,
            &session_task,
            &agent_task,
            &options_task,
            &policy_task,
            &mcp_task,
            &extension_task,
            max_steps_task,
            &submitted,
            &cancel_task,
            &reasoning_cell_task,
        )
        .await;
        match run {
            Ok(result) => ui_task.set_status(&format!("Ready · {} step{}", result.0, if result.0 == 1 { "" } else { "s" })),
            Err(error) => {
                let message = error.to_string();
                if message.contains("Request cancelled") {
                    ui_task.set_status("Cancelled");
                } else {
                    ui_task.push_transcript(format!("error: {message}"));
                    ui_task.set_status("Error");
                }
            }
        }
        busy_task.store(false, Ordering::SeqCst);
        ui_task.set_working(false, ui_task.clone());
        refresh_git_widget(&ui_task);
    });
    Ok(0)
}

/// Build a per-submission agent (mirroring the TS createAgent flow) and drive
/// the tool loop while streaming UI updates from the agent's events.
#[allow(clippy::too_many_lines)]
async fn tui_run_agent(
    ui: &Arc<TerminalUi>,
    session_cell: &Arc<tokio::sync::Mutex<SessionStore>>,
    agent_cell: &Arc<tokio::sync::Mutex<Option<Agent>>>,
    options_cell: &Arc<tokio::sync::Mutex<Option<ProviderOptions>>>,
    policy: &SharedGatePolicy,
    mcp_manager: &Arc<tokio::sync::Mutex<Option<Arc<McpManager>>>>,
    extension_host: &Arc<tokio::sync::Mutex<Option<Arc<crate::extensions::ExtensionHost>>>>,
    max_steps: usize,
    prompt: &str,
    cancel: &Arc<AtomicBool>,
    reasoning_cell: &Arc<tokio::sync::Mutex<crate::types::ReasoningLevel>>,
) -> Result<(usize, String)> {
    let options = options_cell.lock().await.clone().ok_or_else(|| anyhow!("provider is not configured"))?;
    let provider = create_provider(&options)?;
    let context_window = provider.config().context_window;
    let config = default_config(&std::env::current_dir()?, &home_dir());
    let project_trusted = is_project_trusted(&config.cwd, &config.pi_config_dir);
    // Resources: skills/context/system instructions feed the system prompt and
    // the skill tool (same loading as the `run` command path).
    let resources = crate::resources::load_resources(&crate::resources::ResourceLoadOptions {
        cwd: config.cwd.clone(),
        personal_dir: config.pi_config_dir.clone(),
        include_project: true,
        project_trusted,
        package_roots: crate::resources::packages::resolve_package_roots(
            &crate::resources::packages::package_sources_from_settings(&config.pi_config_dir, Some(&config.cwd)),
            &crate::resources::packages::ResolveOptions { cwd: &config.cwd, personal_dir: &config.pi_config_dir, install: false },
            &|_| {},
        ),
    })
    .unwrap_or_else(|error| {
        ui.push_transcript(format!("system: resources failed to load: {error}"));
        Default::default()
    });
    let skills = resources.registry.list(Some(crate::resources::ResourceKind::Skill));
    let mut tools: Vec<Arc<dyn Tool>> = builtin_tools(&config.cwd);
    if !skills.is_empty() {
        tools.push(Arc::new(crate::tools::SkillTool::from_entries(
            skills.iter().map(|entry| (entry.name.clone(), entry.path.clone())).collect(),
        )));
    }
    // Extension tools from the already-started host.
    if let Some(host) = extension_host.lock().await.as_ref() {
        for name in host.tool_names() {
            match host.tool_wrapper(&name) {
                Ok(wrapper) => tools.push(wrapper),
                Err(error) => ui.push_transcript(format!("system: extension tool {name} unavailable: {error}")),
            }
        }
    }
    if let Some(manager) = mcp_manager.lock().await.as_ref() {
        for name in manager.tool_names() {
            if let Some(entry) = manager.get_tool(&name) {
                tools.push(Arc::new(McpTool { entry }));
            }
        }
    }
    // Reuse retained messages so the agent carries context like the TS flow;
    // the leading system message is dropped so the fresh system prompt is the
    // only one (providers concatenate every system message they see).
    let retained = agent_cell
        .lock()
        .await
        .as_ref()
        .map(|agent| agent.messages().to_vec())
        .unwrap_or_default()
        .into_iter()
        .filter(|message| message.role != crate::types::Role::System)
        .collect::<Vec<_>>();
    let mut agent = Agent::new(
        provider,
        tools,
        CompactionOptions {
            on_compaction: Some({
                let session = session_cell.clone();
                Box::new(move |summary, index| {
                    let session = session.clone();
                    let summary = summary.to_string();
                    std::thread::spawn(move || {
                        if let Ok(mut session) = session.try_lock() {
                            let _ = session.append_compaction_from_message_index(&summary, index, 0);
                        }
                    });
                })
            }),
            ..CompactionOptions::default()
        },
        Some(resources.system_prompt),
        max_steps,
        retained,
        Some(mcp_approver(policy)),
        extension_host
            .lock()
            .await
            .as_ref()
            .map(|host| Arc::new(crate::extensions::HostObserver { host: host.clone() }) as Arc<dyn crate::extensions::AgentObserver>),
    );

    let ui_task = ui.clone();
    let session_task = session_cell.clone();
    let mut stream_text = String::new();
    let result = agent
        .run_checked(
            prompt,
            &mut |event| {
                let ui_task = ui_task.clone();
                let session_task = session_task.clone();
                match &event {
                    HarnessEvent::AssistantDelta { text } => {
                        stream_text.push_str(text);
                        stream_text_task(stream_text.clone());
                        ui_task.set_status("Thinking…");
                    }
                    HarnessEvent::CachePlan { plan } => ui_task.set_cache(plan.clone()),
                    HarnessEvent::Usage { prompt_tokens, .. } => {
                        ui_task.set_widget_value("context", &context_widget_value(*prompt_tokens, context_window));
                        /* usage persisted below */
                    }
                    HarnessEvent::ToolStart { call } => {
                        stream_text_task(String::new());
                        ui_task.push_transcript(format!("▣ tool {} {}", call.name, serde_json::to_string(&call.arguments).unwrap_or_default()));
                        ui_task.set_status(&format!("Tool {}: running", call.name));
                    }
                    HarnessEvent::ToolResult { call, output, error } => {
                        if call.name.starts_with("git") {
                            refresh_git_widget(&ui_task);
                        }
                        if call.name == "git_diff" || call.name == "git_show" {
                            ui_task.set_diff(output.clone());
                        }
                        if call.name == "git_apply" || call.name == "git_conflicts" {
                            let conflicts = if output.starts_with("No merge conflicts") || output == "Git patch applied" { None } else { Some(output.clone()) };
                            ui_task.set_conflicts(conflicts);
                        }
                        let content = format!("```{}\n{}\n```", if call.name.starts_with("git") { "diff" } else { "json" }, output);
                        ui_task.push_transcript(format!("▣ tool {}: {}\n{content}", call.name, if error.unwrap_or(false) { "error" } else { "ok" }));
                    }
                    _ => {}
                }
                // Persist selected events to the session.
                if let HarnessEvent::Usage { prompt_tokens, completion_tokens, cached_tokens, prefix_hash, prefix_tokens, suffix_tokens, latency_ms } = event.clone() {
                    std::thread::spawn(move || {
                        if let Ok(mut session) = session_task.try_lock() {
                            let _ = session.append_usage("model", prompt_tokens, completion_tokens, cached_tokens);
                            if let Some(hash) = prefix_hash {
                                let _ = session.append_cache("provider", "model", &hash, prefix_tokens.unwrap_or(0), suffix_tokens.unwrap_or(0), prompt_tokens, completion_tokens, cached_tokens, latency_ms);
                            }
                        }
                    });
                }
            },
            Vec::new(),
            Some(cancel.clone()),
        )
        .await;

    if let Err(error) = result {
        mark_done();
        agent_done_placeholder();
        return Err(error);
    }
    let result = result?;
    // Persist the exchange into the session after the loop (mirror run mode).
    if let Ok(mut session) = session_cell.try_lock() {
        for message in agent.messages() {
            let _ = session.append_message(message);
        }
    }
    mark_done();
    agent_done_placeholder();
    Ok((result.steps, result.text))
}

fn stream_text_task(_text: String) {
    // Streaming deltas already render via push_transcript; retained for parity.
}

fn mark_done() {}
fn agent_done_placeholder() {}


pub async fn run_tui() -> Result<i32> {
    let config = default_config(&std::env::current_dir()?, &home_dir());
    let project_trusted = is_project_trusted(&config.cwd, &config.pi_config_dir);
    let (intents_tx, mut intents_rx) = tokio::sync::mpsc::unbounded_channel::<Intent>();
    let ui = TerminalUi::new(intents_tx.clone());
    crate::tui_interactions::set_active_ui(Some(ui.clone()));
    let mut session = SessionStore::create(&config.cwd)?;
    for message in session.messages().unwrap_or_default() {
        ui.push_transcript(format!("{}: {}", message.role.as_str(), message.content));
    }
    let session_label = session.header.id.chars().take(8).collect::<String>();
    let session_cell = Arc::new(tokio::sync::Mutex::new(session));

    let project_workspace = project_trusted && workspace::has_project_workspace(&config.cwd);
    let layout = workspace::load_workspace(&config.pi_config_dir, &config.cwd, project_trusted);
    ui.set_workspace_config(config.pi_config_dir.clone(), config.cwd.clone(), project_trusted);
    ui.set_workspace(
        &layout,
        vec![
            ("session".to_string(), format!("{session_label} · {} messages", session_cell.lock().await.all_entries().len())),
            ("context".to_string(), "0 tok".to_string()),
            ("model".to_string(), {
                let (settings, _) = load_settings(&config.cwd, &config.pi_config_dir);
                format!("{} · {}", settings.default_provider.clone().unwrap_or_else(|| "openrouter".into()), settings.default_model.clone().unwrap_or_else(|| "default".into()))
            }),
            ("project".to_string(), config.cwd.display().to_string()),
            ("git".to_string(), git_widget_value(&config.cwd)),
        ],
    );
    ui.set_on_workspace_change({
        let pi = config.pi_config_dir.clone();
        let cwd = config.cwd.clone();
        move |layout| {
            let _ = workspace::save_workspace(&pi, &cwd, project_workspace, layout);
        }
    });

    ui.set_completions(vec![
        "/auth ".into(),
        "/login ".into(),
        "/attach ".into(),
        "/compact".into(),
        "/tree".into(),
        "/branch ".into(),
        "/trust ".into(),
        "/mcp ".into(),
        "/diff".into(),
        "/cache".into(),
        "/conflicts".into(),
        "/model ".into(),
        "/reasoning ".into(),
    ]);

    // Provider from settings; failure is shown but not fatal (TS behaves the same).
    let (provider_id, options) = match crate::cli::apply_settings(None, None, None, None, None) {
        Ok((provider_id, options)) => (provider_id, Some(options)),
        Err(error) => {
            ui.push_transcript(format!("system: {error}"));
            (ProviderId::OpenRouter, None)
        }
    };
    ui.push_transcript(format!("system: provider {}", provider_id.as_str()));
    let provider_state: Arc<tokio::sync::Mutex<Option<ProviderOptions>>> = Arc::new(tokio::sync::Mutex::new(options.clone()));
    let policy: SharedGatePolicy = Arc::new(GatePolicy::new(PermissionMode::Ask, &config.pi_config_dir, Vec::new()));
    let tools_at_start = builtin_tools(&config.cwd);
    let initial_agent = match &options {
        Some(options) => match create_provider(options) {
            Ok(provider) => Some(Agent::new(provider, tools_at_start, CompactionOptions::default(), None, 8, Vec::new(), Some(mcp_approver(&policy)), None)),
            Err(error) => {
                ui.push_transcript(format!("system: {error}"));
                None
            }
        },
        None => None,
    };
    let agent_state: Arc<tokio::sync::Mutex<Option<Agent>>> = Arc::new(tokio::sync::Mutex::new(initial_agent));
    let mcp_manager: Arc<tokio::sync::Mutex<Option<Arc<McpManager>>>> = Arc::new(tokio::sync::Mutex::new(None));
    // Extension host: trusted project `extensions/` directory (same discovery
    // as the `run` command); tools, hooks, and slash commands become live.
    let extension_state: Arc<tokio::sync::Mutex<Option<Arc<crate::extensions::ExtensionHost>>>> = Arc::new(tokio::sync::Mutex::new(None));
    let extension_paths = crate::extensions::discover_extensions(&config.cwd, project_trusted);
    if !extension_paths.is_empty() {
        let host = crate::extensions::ExtensionHost::new(extension_paths, &config.cwd);
        match host.start().await {
            Ok(_) => {
                let tool_names = host.tool_names();
                let commands = host.command_list();
                ui.push_transcript(format!("system: extensions loaded ({} tool{})", tool_names.len(), if tool_names.len() == 1 { "" } else { "s" }));
                for command in &commands {
                    ui.push_transcript(format!("system: extension command /{} — {}", command.name, command.description));
                }
                let mut completions = ui.completions();
                for command in &commands {
                    completions.push(format!("/{} ", command.name));
                }
                ui.set_completions(completions);
                *extension_state.lock().await = Some(host);
            }
            Err(error) => ui.push_transcript(format!("system: extension host failed: {error}")),
        }
    }
    let reasoning_cell: Arc<tokio::sync::Mutex<crate::types::ReasoningLevel>> = Arc::new(tokio::sync::Mutex::new(options.as_ref().and_then(|options| options.reasoning).unwrap_or_default()));
    if let Some(level) = options.as_ref().and_then(|options| options.reasoning) {
        if let Some(agent) = agent_state.lock().await.as_mut() {
            agent.set_reasoning(level);
        }
    }
    let cancel = Arc::new(AtomicBool::new(false));
    let busy = Arc::new(AtomicBool::new(false));

    // Enter the interactive loop on a spawned task; the coordinator below owns intents.
    let ui_loop = ui.clone();
    let ui_task = tokio::spawn(async move { ui_loop.start(ui_loop.clone()).await });

    while let Some(intent) = intents_rx.recv().await {
        match intent {
            Intent::Quit => {
                cancel.store(true, Ordering::SeqCst);
                break;
            }
            Intent::Cancel => {
                cancel.store(true, Ordering::SeqCst);
                if !busy.load(Ordering::SeqCst) {
                    ui.set_status("Nothing running");
                }
            }
            Intent::Submit(text) => {
                handle_tui_submit(&ui, &text, &session_cell, &provider_state, &agent_state, &policy, 8, &mcp_manager, &extension_state, &reasoning_cell, &cancel, &busy).await?;
            }
        }
    }
    if let Some(mcp) = mcp_manager.lock().await.as_ref() {
        mcp.close().await;
    }
    if let Some(host) = extension_state.lock().await.as_ref() {
        host.stop().await;
    }
    let _ = ui_task.await;
    Ok(0)
}

fn config_path() -> std::path::PathBuf {
    default_config(&std::env::current_dir().unwrap_or_default(), &home_dir()).cwd
}

async fn current_model_label(provider_state: &Arc<tokio::sync::Mutex<Option<ProviderOptions>>>) -> String {
    provider_state
        .lock()
        .await
        .clone()
        .and_then(|options| options.model)
        .unwrap_or_else(|| "settings default".into())
}

fn hl_harness_tui_theme_list() -> [crate::tui::theme::Theme; 2] {
    crate::tui::theme::themes()
}

fn hl_harness_tui_theme_json(content: &str) -> Option<crate::tui::theme::ThemeColors> {
    crate::tui::theme::theme_from_json(content)
}

fn hl_harness_tui_set_theme(colors: crate::tui::theme::ThemeColors) {
    crate::tui::theme::set_active(colors);
}
