//! Extension host, ported from `src/extensions/host.ts`.
//!
//! Extensions remain JavaScript/TypeScript modules running in an isolated Node
//! child process; the parent↔child protocol is byte-compatible with the
//! TypeScript harness (`ready`/`tool`/`command`/`provider`/`tool_result`/…).

use crate::types::{RiskLevel, ToolCall, ToolDeclaration};
use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;
use std::collections::{BTreeMap, HashMap};

pub const HOST_SCRIPT: &str = include_str!("../../assets/host-process.mjs");

#[derive(Clone, Debug, PartialEq)]
pub struct ExtensionToolDecl {
    pub declaration: ToolDeclaration,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExtensionCommand {
    pub name: String,
    pub description: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExtensionProvider {
    pub id: String,
    pub base_url: String,
    pub model: String,
    pub api_key_env: Option<String>,
    pub headers: Vec<(String, String)>,
}

/// Hooks the agent consults for extension behavior.
#[async_trait]
pub trait AgentObserver: Send + Sync {
    async fn before_tool(&self, call: &ToolCall) -> Result<Value>;
    async fn after_tool(&self, call: &ToolCall, output: &str, error: bool) -> Result<Option<String>>;
    async fn emit(&self, event: &str, payload: Value) -> Result<()>;
    fn commands(&self) -> Vec<ExtensionCommand> {
        Vec::new()
    }
}

struct HostState {
    child: Option<tokio::process::Child>,
    writer: Option<tokio::sync::mpsc::UnboundedSender<String>>,
    pending: HashMap<String, oneshot::Sender<Value>>,
    tools: BTreeMap<String, ExtensionToolDecl>,
    commands: BTreeMap<String, ExtensionCommand>,
    providers: BTreeMap<String, ExtensionProvider>,
}

#[derive(Clone)]
pub struct ExtensionHost {
    paths: Arc<Vec<String>>,
    state: Arc<Mutex<HostState>>,
    ready: Arc<AtomicBool>,
    started: Arc<AtomicBool>,
    next_id: Arc<AtomicU64>,
}

impl ExtensionHost {
    pub fn new(paths: Vec<String>, _cwd: &Path) -> Arc<Self> {
        Arc::new(Self {
            paths: Arc::new(paths),
            state: Arc::new(Mutex::new(HostState { child: None, writer: None, pending: HashMap::new(), tools: BTreeMap::new(), commands: BTreeMap::new(), providers: BTreeMap::new() })),
            ready: Arc::new(AtomicBool::new(false)),
            started: Arc::new(AtomicBool::new(false)),
            next_id: Arc::new(AtomicU64::new(0)),
        })
    }

    pub async fn start(self: &Arc<Self>) -> Result<()> {
        if self.paths.is_empty() {
            return Ok(());
        }
        let script = host_script_path()?;
        if let Some(parent) = script.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&script, HOST_SCRIPT)?;

        let mut command = tokio::process::Command::new("node");
        // --experimental-strip-types lets .ts extensions load on Node 22/23.
        command
            .args(["--experimental-strip-types", script.to_string_lossy().as_ref()])
            .args(self.paths.iter().map(String::as_str))
            .current_dir(&self.cwd())
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = command.spawn().map_err(|error| anyhow!("Extension host failed to start: {error}"))?;
        let stdin = child.stdin.take().ok_or_else(|| anyhow!("Extension host stdin unavailable"))?;
        let stdout = child.stdout.take().ok_or_else(|| anyhow!("Extension host stdout unavailable"))?;
        if let Some(mut stderr) = child.stderr.take() {
            tokio::spawn(async move {
                use tokio::io::AsyncReadExt;
                let mut buffer = String::new();
                let mut chunks = [0u8; 4096];
                loop {
                    match stderr.read(&mut chunks).await {
                        Ok(0) | Err(_) => break,
                        Ok(read) => {
                            buffer.push_str(&String::from_utf8_lossy(&chunks[..read]));
                            while let Some(newline) = buffer.find('\n') {
                                let line: String = buffer.drain(..=newline).collect();
                                eprint!("[extension] {line}");
                            }
                        }
                    }
                }
            });
        }
        let writer = spawn_writer(stdin);
        let ready = self.ready.clone();
        let state = self.state.clone();
        spawn_reader(stdout, state, ready);
        {
            let mut state = self.state.lock().unwrap();
            state.child = Some(child);
            state.writer = Some(writer);
        }
        self.started.store(true, Ordering::SeqCst);
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !self.ready.load(Ordering::SeqCst) {
            if std::time::Instant::now() > deadline {
                bail!("Extension host startup timed out");
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        Ok(())
    }

    pub async fn request(self: &Arc<Self>, message: Value) -> Result<Value> {
        if !self.started.load(Ordering::SeqCst) || !self.ready.load(Ordering::SeqCst) {
            bail!("Extension host is not ready");
        }
        let id = format!("extension_{}", self.next_id.fetch_add(1, Ordering::SeqCst) + 1);
        let (sender, receiver) = oneshot::channel();
        {
            let mut state = self.state.lock().unwrap();
            state.pending.insert(id.clone(), sender);
            if let Some(writer) = &state.writer {
                let _ = writer.send(serde_json::Value::Object({
                    let mut map = message.as_object().cloned().unwrap_or_default();
                    map.insert("id".to_string(), json!(id));
                    map
                }).to_string());
            } else {
                state.pending.remove(&id);
                bail!("Extension host is not ready");
            }
        }
        match tokio::time::timeout(std::time::Duration::from_secs(120), receiver).await {
            Ok(Ok(response)) => {
                if let Some(error) = response["error"].as_str() {
                    bail!("{error}");
                }
                Ok(response)
            }
            Ok(Err(_)) => bail!("Extension host dropped the request"),
            Err(_) => bail!("Extension host request timed out"),
        }
    }

    pub async fn before_tool(self: &Arc<Self>, call: &ToolCall) -> Result<Option<Value>> {
        let response = self.request(json!({"type": "hook", "name": "tool_call", "args": call.arguments})).await?;
        let hook = response["payload"].clone();
        let relevant = hook.as_object().map(|_map| hook.get("block").and_then(Value::as_bool).unwrap_or(false) || hook.get("args").is_some()).unwrap_or(false);
        Ok(if relevant { Some(hook) } else { None })
    }

    pub async fn after_tool(self: &Arc<Self>, call: &ToolCall, output: &str, _error: bool) -> Result<Option<String>> {
        let response = self.request(json!({"type": "hook", "name": "tool_result", "args": call.arguments, "text": output})).await?;
        Ok(response["payload"]["output"].as_str().map(str::to_string))
    }

    pub async fn call_command(self: &Arc<Self>, name: &str, text: &str) -> Result<String> {
        let response = self.request(json!({"type": "command_call", "name": name, "text": text})).await?;
        Ok(response["output"].as_str().unwrap_or_default().to_string())
    }

    pub async fn emit(self: &Arc<Self>, event: &str, payload: Value) -> Result<()> {
        let writer = self.state.lock().unwrap().writer.clone();
        if let Some(writer) = writer {
            let _ = writer.send(json!({"type": "event", "event": event, "payload": payload}).to_string());
        }
        Ok(())
    }

    pub fn tool_names(&self) -> Vec<String> {
        self.state.lock().unwrap().tools.keys().cloned().collect()
    }

    pub fn tool_declaration(&self, name: &str) -> Option<ExtensionToolDecl> {
        self.state.lock().unwrap().tools.get(name).cloned()
    }

    pub fn command_list(&self) -> Vec<ExtensionCommand> {
        self.state.lock().unwrap().commands.values().cloned().collect()
    }

    pub fn provider_list(&self) -> Vec<ExtensionProvider> {
        self.state.lock().unwrap().providers.values().cloned().collect()
    }

    pub fn tool_wrapper(self: &Arc<Self>, name: &str) -> Result<Arc<ExtensionTool>> {
        let cached = self.tool_declaration(name).ok_or_else(|| anyhow!("Unknown extension tool: {name}"))?;
        let declaration = Box::leak(Box::new(cached.declaration));
        Ok(Arc::new(ExtensionTool { name: name.to_string(), declaration, host: self.clone() }))
    }

    pub async fn stop(&self) -> Result<()> {
        let child = {
            let mut state = self.state.lock().unwrap();
            if let Some(writer) = &state.writer {
                let _ = writer.send(json!({"type": "shutdown"}).to_string());
            }
            state.child.take()
        };
        if let Some(mut child) = child {
            let _ = child.wait().await;
        }
        self.ready.store(false, Ordering::SeqCst);
        self.started.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn cwd(&self) -> PathBuf {
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
    }
}

/// Adapter exposing the host as the agent's observer.
pub struct HostObserver {
    pub host: Arc<ExtensionHost>,
}

#[async_trait]
impl AgentObserver for HostObserver {
    async fn before_tool(&self, call: &ToolCall) -> Result<Value> {
        self.host.before_tool(call).await?.map_or_else(|| Ok(json!({})), |value| Ok(value))
    }

    async fn after_tool(&self, call: &ToolCall, output: &str, error: bool) -> Result<Option<String>> {
        self.host.after_tool(call, output, error).await
    }

    async fn emit(&self, event: &str, payload: Value) -> Result<()> {
        self.host.emit(event, payload).await
    }

    fn commands(&self) -> Vec<ExtensionCommand> {
        self.host.command_list()
    }
}

/// Tool wrapper so agent-loop tools route through the child process.
pub struct ExtensionTool {
    pub name: String,
    pub declaration: &'static ToolDeclaration,
    pub host: Arc<ExtensionHost>,
}

#[async_trait]
impl crate::tools::Tool for ExtensionTool {
    fn declaration(&self) -> &ToolDeclaration {
        self.declaration
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Extension
    }

    async fn execute(&self, args: &Value) -> Result<String> {
        let response = self.host.request(json!({"type": "tool_call", "name": self.name, "args": args})).await?;
        if let Some(error) = response["error"].as_str() {
            bail!("{error}");
        }
        Ok(response["output"].as_str().unwrap_or_default().to_string())
    }
}

/// Discover extension entry files from the trusted project `extensions/` dir.
pub fn discover_extensions(cwd: &Path, project_trusted: bool) -> Vec<String> {
    let mut paths = Vec::new();
    if project_trusted {
        let directory = cwd.join("extensions");
        if let Ok(entries) = std::fs::read_dir(&directory) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if name.ends_with(".js") || name.ends_with(".mjs") || name.ends_with(".ts") {
                    paths.push(entry.path().to_string_lossy().to_string());
                }
            }
        }
    }
    paths.sort();
    paths
}

fn host_script_path() -> Result<PathBuf> {
    Ok(std::env::var("HARNESS_EXTENSION_HOST")
        .map(PathBuf::from)
        .unwrap_or_else(|_| crate::config::home_dir().join(".pi").join("agent").join("host-process.mjs")))
}

fn spawn_writer(mut stdin: tokio::process::ChildStdin) -> tokio::sync::mpsc::UnboundedSender<String> {
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        while let Some(line) = receiver.recv().await {
            if stdin.write_all(format!("{line}\n").as_bytes()).await.is_err() {
                break;
            }
            let _ = stdin.flush().await;
        }
    });
    sender
}

fn spawn_reader<R: tokio::io::AsyncRead + Unpin + Send + 'static>(stream: R, state: Arc<Mutex<HostState>>, ready: Arc<AtomicBool>) {
    tokio::spawn(async move {
        use tokio::io::AsyncBufReadExt;
        let mut reader = tokio::io::BufReader::new(stream);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let Ok(message) = serde_json::from_str::<serde_json::Value>(line.trim()) else { continue };
            #[cfg(feature = "extension_debug")]
            eprintln!("[ext-debug] {message_type}", message_type = message["type"]);
            let mut state = state.lock().unwrap();
            let message_type = message["type"].as_str().unwrap_or_default().to_string();
            match message_type.as_str() {
                "ready" => ready.store(true, Ordering::SeqCst),
                "tool" => {
                    let tool = &message["tool"];
                    let name = tool["name"].as_str().unwrap_or_default().to_string();
                    let declaration = ToolDeclaration {
                        name: name.clone(),
                        description: tool["description"].as_str().unwrap_or_default().to_string(),
                        parameters: tool["parameters"].clone(),
                    };
                    state.tools.insert(name, ExtensionToolDecl { declaration });
                }
                "command" => {
                    let command = &message["command"];
                    state.commands.insert(
                        command["name"].as_str().unwrap_or_default().to_string(),
                        ExtensionCommand {
                            name: command["name"].as_str().unwrap_or_default().to_string(),
                            description: command["description"].as_str().unwrap_or_default().to_string(),
                        },
                    );
                }
                "provider" => {
                    let provider = &message["provider"];
                    state.providers.insert(
                        provider["id"].as_str().unwrap_or_default().to_string(),
                        ExtensionProvider {
                            id: provider["id"].as_str().unwrap_or_default().to_string(),
                            base_url: provider["baseUrl"].as_str().unwrap_or_default().to_string(),
                            model: provider["model"].as_str().unwrap_or_default().to_string(),
                            api_key_env: provider["apiKeyEnv"].as_str().map(str::to_string),
                            headers: provider["headers"].as_object().map(|headers| headers.iter().map(|(key, value)| (key.clone(), value.as_str().unwrap_or_default().to_string())).collect()).unwrap_or_default(),
                        },
                    );
                }
                "extension_error" => {
                    let error = message["error"].as_str().unwrap_or("Extension error").to_string();
                    let pending = std::mem::take(&mut state.pending);
                    for sender in pending.into_values() {
                        let _ = sender.send(serde_json::json!({"error": error.clone()}));
                    }
                }
                _ => {
                    let id = message["id"].as_str().unwrap_or_default().to_string();
                    if let Some(sender) = state.pending.remove(&id) {
                        let _ = sender.send(message);
                    }
                }
            }
        }
    });
}
