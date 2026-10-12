//! Browser automation through the Chrome DevTools Protocol, ported from the
//! `browser_*` tools in `src/tools/index.ts` (no Playwright dependency).

use super::{clip, Tool, OUTPUT_LIMIT};
use crate::types::{RiskLevel, ToolDeclaration};
use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

type WsStream = tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type WsMessage = tokio_tungstenite::tungstenite::protocol::Message;

fn static_declaration(name: &str, description: &str, parameters: serde_json::Value) -> ToolDeclaration {
    ToolDeclaration { name: name.to_string(), description: description.to_string(), parameters }
}

pub fn leaked_declaration(name: &str, description: &str, parameters: serde_json::Value) -> &'static ToolDeclaration {
    Box::leak(Box::new(static_declaration(name, description, parameters)))
}

pub struct CdpSession {
    next_id: AtomicU64,
    pending: Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Value>>>>,
    write: Arc<tokio::sync::Mutex<Option<futures_util::stream::SplitSink<WsStream, WsMessage>>>>,
    closed: Arc<AtomicBool>,
}

/// Registry carrying the connected CDP session and the pid of a harness-launched browser.
#[derive(Default)]
pub struct BrowserRegistry {
    session: Arc<Mutex<Option<Arc<CdpSession>>>>,
    launched_pid: Arc<Mutex<Option<u32>>>,
}

fn cdp_base() -> String {
    std::env::var("HARNESS_CDP_URL").unwrap_or_else(|_| "http://127.0.0.1:9222".into()).trim_end_matches('/').to_string()
}

async fn list_browser_targets() -> Result<Vec<Value>> {
    let response = reqwest::Client::new().get(format!("{}/json/list", cdp_base())).send().await?;
    let status = response.status();
    if !status.is_success() {
        bail!("CDP endpoint failed: HTTP {}", status.as_u16());
    }
    Ok(response.json().await?)
}

impl CdpSession {
    async fn connect(session_url: &str, target_id: Option<&str>) -> Result<Arc<CdpSession>> {
        let (stream, _) = tokio_tungstenite::connect_async(session_url)
            .await
            .map_err(|error| anyhow!("CDP WebSocket connection failed: {error}"))?;
        let (write, mut read) = stream.split();
        let session = Arc::new(CdpSession {
            next_id: AtomicU64::new(0),
            pending: Arc::new(Mutex::new(HashMap::new())),
            write: Arc::new(tokio::sync::Mutex::new(Some(write))),
            closed: Arc::new(AtomicBool::new(false)),
        });
        let pending = session.pending.clone();
        let closed = session.closed.clone();
        tokio::spawn(async move {
            while let Some(message) = read.next().await {
                let Ok(message) = message else { break };
                let tokio_tungstenite::tungstenite::Message::Text(text) = message else { continue };
                let Ok(value) = serde_json::from_str::<Value>(&text) else { continue };
                let Some(id) = value["id"].as_u64() else { continue };
                let resolved = pending.lock().unwrap().remove(&id);
                let Some(sender) = resolved else { continue };
                if value["error"].is_object() {
                    let message = value["error"]["message"].as_str().unwrap_or("CDP command failed").to_string();
                    let _ = sender.send(json!({"__cdp_error": message}));
                } else {
                    let _ = sender.send(value["result"].clone());
                }
            }
            closed.store(true, Ordering::SeqCst);
            // Cancel all waiters by dropping them.
            pending.lock().unwrap().clear();
        });
        let _ = target_id;
        Ok(session)
    }

    pub async fn command(&self, method: &str, params: Value) -> Result<Value> {
        if self.closed.load(Ordering::SeqCst) {
            bail!("Browser session is closed; run browser_open first");
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let (sender, receiver) = tokio::sync::oneshot::channel::<Value>();
        self.pending.lock().unwrap().insert(id, sender);
        let payload = json!({"id": id, "method": method, "params": params}).to_string();
        let send_result = {
            let mut guard = self.write.lock().await;
            match guard.as_mut() {
                Some(sink) => sink.send(WsMessage::Text(payload)).await.is_ok(),
                None => false,
            }
        };
        if !send_result {
            self.closed.store(true, Ordering::SeqCst);
            bail!("Browser session is closed; run browser_open first");
        }
        match tokio::time::timeout(Duration::from_secs(30), receiver).await {
            Ok(Ok(value)) => {
                if let Some(message) = value["__cdp_error"].as_str() {
                    bail!(message.to_string());
                }
                Ok(value)
            }
            Ok(Err(_)) | Err(_) => bail!("CDP command timed out or session closed: {method}"),
        }
    }

    pub async fn close_stream(&self) {
        let mut guard = self.write.lock().await;
        if let Some(mut sink) = guard.take() {
            let _ = sink.close().await;
        }
        drop(guard);
        self.closed.store(true, Ordering::SeqCst);
        self.pending.lock().unwrap().clear();
    }
}

impl BrowserRegistry {
    pub async fn connect(&self, force: bool, target_id: Option<&str>) -> Result<Arc<CdpSession>> {
        if !force {
            if let Some(session) = self.session.lock().unwrap().clone() {
                return Ok(session);
            }
        }
        if force {
            let existing = self.session.lock().unwrap().take();
            if let Some(existing) = existing {
                existing.close_stream().await;
            }
        }
        let targets = list_browser_targets().await?;
        let target = targets.iter().find(|candidate| {
            candidate["type"] == "page"
                && candidate["webSocketDebuggerUrl"].is_string()
                && (target_id.is_none() || candidate["id"] == target_id.unwrap_or_default())
        });
        let Some(target) = target else {
            if target_id.is_some() {
                bail!("CDP page target not found: {}", target_id.unwrap_or_default());
            }
            bail!("CDP endpoint has no page target; launch a browser with remote debugging");
        };
        let url = target["webSocketDebuggerUrl"].as_str().unwrap_or_default().to_string();
        let session = CdpSession::connect(&url, target_id).await?;
        *self.session.lock().unwrap() = Some(session.clone());
        Ok(session)
    }

    pub async fn launch(&self) -> Result<String> {
        let configured = std::env::var("HARNESS_BROWSER_COMMAND").ok().filter(|value| !value.is_empty());
        let candidates: Vec<String> = match configured {
            Some(command) => vec![command],
            None => match std::env::consts::OS {
                "macos" => vec!["/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into(), "google-chrome".into(), "chromium".into(), "chromium-browser".into()],
                _ => vec!["google-chrome".into(), "chromium".into(), "chromium-browser".into(), "brave-browser".into()],
            },
        };
        let command = candidates.iter().find(|candidate| {
            if candidate.starts_with('/') {
                Path::new(candidate).exists()
            } else {
                executable_on_path(candidate).is_some()
            }
        });
        let command = command.ok_or_else(|| anyhow!("No Chrome/Chromium executable found; set HARNESS_BROWSER_COMMAND"))?;
        let url = url::Url::parse(&cdp_base())?;
        let port = url.port().unwrap_or(9222);
        let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/"));
        let profile = home.join(".pi").join("agent").join("browser-profile");
        std::fs::create_dir_all(&profile)?;
        let headless: Vec<String> = if std::env::var("HARNESS_BROWSER_HEADLESS").as_deref() == Ok("1") { vec!["--headless=new".into(), "--disable-gpu".into()] } else { Vec::new() };
        let mut child_args: Vec<String> = vec![format!("--remote-debugging-port={port}"), format!("--user-data-dir={}", profile.display()), "--no-first-run".into(), "--no-default-browser-check".into()];
        child_args.extend(headless);
        let mut command_spawn = tokio::process::Command::new(command);
        command_spawn.args(&child_args).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null());
        let child = command_spawn.spawn()?;
        let pid = child.id();
        *self.launched_pid.lock().unwrap() = pid;
        let base = cdp_base();
        for _attempt in 0..50 {
            if list_browser_targets().await.is_ok() {
                return Ok(format!("Launched browser (pid {})", pid.map(|pid| pid.to_string()).unwrap_or_else(|| "unknown".into())));
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        bail!("Browser launched but CDP endpoint did not become ready")
    }

    pub fn session(&self) -> Option<Arc<CdpSession>> {
        self.session.lock().unwrap().clone()
    }

    pub async fn disconnect(&self) {
        let session = self.session.lock().unwrap().take();
        if let Some(session) = session {
            session.close_stream().await;
        }
    }

    pub async fn shutdown(&self) -> String {
        self.disconnect().await;
        let pid = self.launched_pid.lock().unwrap().take();
        if let Some(pid) = pid {
            unsafe {
                kill_c(pid as i32, 15);
            }
            return "Browser shut down".to_string();
        }
        "Browser shut down".to_string()
    }
}

extern "C" {
    #[link_name = "kill"]
    fn kill_c(pid: i32, signal: i32) -> i32;
}

use super::runtime::executable_on_path;

// ---------------------------------------------------------------------------
// Tools
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq)]
enum BrowserKind {
    Launch,
    Reconnect,
    Tabs,
    Select,
    Open,
    Eval,
    Click,
    Text,
    Screenshot,
    Close,
    Shutdown,
}

pub struct BrowserTool {
    kind: BrowserKind,
    registry: Arc<BrowserRegistry>,
    cwd: PathBuf,
    declaration: &'static ToolDeclaration,
}

const TOOLS: [(BrowserKind, &str, &str, &str, RiskLevel); 11] = [
    (BrowserKind::Launch, "browser_launch", "Launch a Chrome/Chromium browser with a persistent CDP profile and wait until its debugging endpoint is ready.", r#"{"type":"object","properties":{},"additionalProperties":false}"#, RiskLevel::Shell),
    (BrowserKind::Reconnect, "browser_reconnect", "Disconnect and reconnect to the current first CDP page target.", r#"{"type":"object","properties":{},"additionalProperties":false}"#, RiskLevel::Network),
    (BrowserKind::Tabs, "browser_tabs", "List page targets exposed by the configured CDP endpoint.", r#"{"type":"object","properties":{},"additionalProperties":false}"#, RiskLevel::Network),
    (BrowserKind::Select, "browser_select", "Connect the harness to a specific CDP page target id.", r#"{"type":"object","properties":{"id":{"type":"string"}},"required":["id"],"additionalProperties":false}"#, RiskLevel::Network),
    (BrowserKind::Open, "browser_open", "Connect to the first page exposed by a Chrome DevTools Protocol endpoint and optionally navigate to an HTTP(S) URL.", r#"{"type":"object","properties":{"url":{"type":"string"}},"additionalProperties":false}"#, RiskLevel::Network),
    (BrowserKind::Eval, "browser_eval", "Evaluate JavaScript in the connected browser page and return a JSON-serializable result.", r#"{"type":"object","properties":{"expression":{"type":"string"}},"required":["expression"],"additionalProperties":false}"#, RiskLevel::Network),
    (BrowserKind::Click, "browser_click", "Click the first element matching a CSS selector in the connected browser page.", r#"{"type":"object","properties":{"selector":{"type":"string"}},"required":["selector"],"additionalProperties":false}"#, RiskLevel::Network),
    (BrowserKind::Text, "browser_text", "Read bounded visible text from the connected browser page.", r#"{"type":"object","properties":{},"additionalProperties":false}"#, RiskLevel::Network),
    (BrowserKind::Screenshot, "browser_screenshot", "Capture the connected browser page to a project-local PNG file.", r#"{"type":"object","properties":{"path":{"type":"string"}},"required":["path"],"additionalProperties":false}"#, RiskLevel::Write),
    (BrowserKind::Close, "browser_close", "Disconnect the harness from the browser page without shutting down the browser.", r#"{"type":"object","properties":{},"additionalProperties":false}"#, RiskLevel::Network),
    (BrowserKind::Shutdown, "browser_shutdown", "Disconnect and shut down a browser previously started by browser_launch.", r#"{"type":"object","properties":{},"additionalProperties":false}"#, RiskLevel::Shell),
];

#[async_trait]
impl Tool for BrowserTool {
    fn declaration(&self) -> &ToolDeclaration {
        self.declaration
    }

    fn risk(&self) -> RiskLevel {
        TOOLS.iter().find(|entry| entry.0 == self.kind).unwrap().4
    }

    async fn execute(&self, args: &Value) -> Result<String> {
        let registry = &self.registry;
        match self.kind {
            BrowserKind::Launch => Ok(registry.launch().await?),
            BrowserKind::Reconnect => {
                registry.connect(true, None).await?;
                Ok("Browser reconnected".to_string())
            }
            BrowserKind::Tabs => {
                let targets = list_browser_targets().await?;
                let pages: Vec<Value> = targets
                    .iter()
                    .filter(|target| target["type"] == "page")
                    .map(|target| json!({"id": target["id"], "title": target["title"], "url": target["url"]}))
                    .collect();
                Ok(clip(&serde_json::to_string_pretty(&pages)?, OUTPUT_LIMIT))
            }
            BrowserKind::Select => {
                let id = args.get("id").and_then(Value::as_str).unwrap_or_default().to_string();
                if id.is_empty() {
                    bail!("browser_select requires an id");
                }
                registry.connect(true, Some(&id)).await?;
                Ok(format!("Selected page {id}"))
            }
            BrowserKind::Open => {
                let session = registry.connect(false, None).await?;
                let value = args.get("url").and_then(Value::as_str).unwrap_or_default().to_string();
                if !value.is_empty() {
                    let parsed = url::Url::parse(&value)?;
                    if parsed.scheme() != "http" && parsed.scheme() != "https" {
                        bail!("browser_open only supports http(s) URLs");
                    }
                    session.command("Page.navigate", json!({"url": value})).await?;
                }
                Ok("Browser connected".to_string())
            }
            BrowserKind::Eval => {
                let expression = args.get("expression").and_then(Value::as_str).unwrap_or_default().to_string();
                if expression.is_empty() {
                    bail!("browser_eval requires an expression");
                }
                let session = registry.session().ok_or_else(|| anyhow!("No browser session; run browser_open first"))?;
                let result = session.command("Runtime.evaluate", json!({"expression": expression, "awaitPromise": true, "returnByValue": true})).await?;
                if result["exceptionDetails"].is_object() {
                    bail!("Browser evaluation threw an exception");
                }
                Ok(clip(&serde_json::to_string_pretty(&result["result"]["value"])?, OUTPUT_LIMIT))
            }
            BrowserKind::Click => {
                let selector = args.get("selector").and_then(Value::as_str).unwrap_or_default().to_string();
                if selector.is_empty() {
                    bail!("browser_click requires a selector");
                }
                let expression = format!("(() => {{ const element = document.querySelector({}); if (!element) return false; element.click(); return true; }})()", serde_json::to_string(&selector)?);
                let session = registry.session().ok_or_else(|| anyhow!("No browser session; run browser_open first"))?;
                let result = session.command("Runtime.evaluate", json!({"expression": expression, "awaitPromise": true, "returnByValue": true})).await?;
                if result["result"]["value"] != json!(true) {
                    bail!("No clickable element matched {selector}");
                }
                Ok(format!("Clicked {selector}"))
            }
            BrowserKind::Text => {
                let session = registry.session().ok_or_else(|| anyhow!("No browser session; run browser_open first"))?;
                let result = session.command("Runtime.evaluate", json!({"expression": "document.body?.innerText ?? ''", "awaitPromise": true, "returnByValue": true})).await?;
                Ok(clip(result["result"]["value"].as_str().unwrap_or_default(), OUTPUT_LIMIT))
            }
            BrowserKind::Screenshot => {
                let path = args.get("path").and_then(Value::as_str).unwrap_or("browser-screenshot.png").to_string();
                let file = super::within(&self.cwd, &path)?;
                let session = registry.session().ok_or_else(|| anyhow!("No browser session; run browser_open first"))?;
                let result = session.command("Page.captureScreenshot", json!({"format": "png"})).await?;
                let data = result["data"].as_str().unwrap_or_default();
                if data.is_empty() {
                    bail!("CDP screenshot returned no data");
                }
                let bytes = base64_decode(data)?;
                if let Some(parent) = file.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&file, bytes)?;
                Ok(format!("Wrote {path}"))
            }
            BrowserKind::Close => {
                registry.disconnect().await;
                Ok("Browser disconnected".to_string())
            }
            BrowserKind::Shutdown => Ok(registry.shutdown().await),
        }
    }
}

fn base64_decode(value: &str) -> Result<Vec<u8>> {
    fn value_of(byte: u8) -> i32 {
        match byte {
            b'A'..=b'Z' => (byte - b'A') as i32,
            b'a'..=b'z' => (byte - b'a' + 26) as i32,
            b'0'..=b'9' => (byte - b'0' + 52) as i32,
            b'+' => 62,
            b'/' => 63,
            _ => -1,
        }
    }
    let mut out = Vec::new();
    let mut buffer: u32 = 0;
    let mut bits = 0;
    for byte in value.bytes() {
        if matches!(byte, b'=' | b'\n' | b'\r') {
            continue;
        }
        let digit = value_of(byte);
        if digit < 0 {
            bail!("Invalid base64 data");
        }
        buffer = (buffer << 6) | digit as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
        }
    }
    Ok(out)
}

impl BrowserRegistry {
    pub fn tools(&self, cwd: &Path) -> Vec<Arc<dyn Tool>> {
        let registry = Arc::new(self.clone_handle());
        let cwd = cwd.to_path_buf();
        TOOLS
            .iter()
            .map(|(kind, name, description, parameters, _)| {
                let declaration: &'static ToolDeclaration = Box::leak(Box::new(ToolDeclaration {
                    name: name.to_string(),
                    description: description.to_string(),
                    parameters: serde_json::from_str::<Value>(parameters).unwrap_or(json!({})),
                }));
                Arc::new(BrowserTool { kind: *kind, registry: registry.clone(), cwd: cwd.clone(), declaration }) as Arc<dyn Tool>
            })
            .collect()
    }

    fn clone_handle(&self) -> BrowserRegistry {
        BrowserRegistry { session: self.session.clone(), launched_pid: self.launched_pid.clone() }
    }
}
