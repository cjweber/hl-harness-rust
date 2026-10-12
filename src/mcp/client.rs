//! MCP client, ported from `src/mcp/client.ts`:
//! configuration files, stdio + Streamable-HTTP transports (JSON-RPC 2.0),
//! tool/resource/prompt access, OAuth with PKCE persistence, and the manager
//! that exposes remote tools as `mcp__<server>__<tool>`.

use crate::types::ToolDeclaration;
use anyhow::{anyhow, bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub const PROTOCOL_VERSION: &str = "2024-11-05";
const CLIENT_NAME: &str = "hl-harness";
const CLIENT_VERSION: &str = "0.1.0";

/// Server→client interaction surface (the TUI will supply interactive handlers).
pub trait McpInteractions: Send + Sync {
    fn elicit(&self, request: Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send>>;
    fn sample(&self, request: Value) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Value>> + Send>>;
}

// ---------------------------------------------------------------------------
// Server configuration
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct McpOAuthConfig {
    pub client_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_secret: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect_uri: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub token_file: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase", default)]
pub struct McpServerConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<BTreeMap<String, String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth: Option<McpOAuthConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

fn server_file(cwd: &Path, project: bool, config_dir: &Path) -> PathBuf {
    if project {
        cwd.join(".pi").join("mcp.json")
    } else {
        config_dir.join("mcp.json")
    }
}

fn load_mcp_file(file: &Path) -> Result<BTreeMap<String, McpServerConfig>> {
    let Ok(content) = std::fs::read_to_string(file) else { return Ok(BTreeMap::new()) };
    let value: Value = serde_json::from_str(&content).map_err(|_| anyhow!("Invalid MCP configuration: {}", file.display()))?;
    let servers = if let Some(nested) = value.get("mcpServers") { nested.clone() } else { value };
    serde_json::from_value(servers).map_err(|_| anyhow!("Invalid MCP configuration: {}", file.display()))
}

pub fn load_mcp_servers(cwd: &Path, config_dir: &Path, project_trusted: bool) -> Result<BTreeMap<String, McpServerConfig>> {
    let mut servers = load_mcp_file(&server_file(cwd, false, config_dir))?;
    if project_trusted {
        servers.extend(load_mcp_file(&server_file(cwd, true, config_dir))?);
    }
    Ok(servers)
}

pub fn save_mcp_servers(cwd: &Path, config_dir: &Path, project: bool, servers: &BTreeMap<String, McpServerConfig>) -> Result<()> {
    let file = server_file(cwd, project, config_dir);
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&file, serde_json::to_string_pretty(&json!({"mcpServers": servers}))? + "\n")?;
    Ok(())
}

pub fn sanitize_mcp_name(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

pub fn add_mcp_server(cwd: &Path, config_dir: &Path, project: bool, name: &str, server: McpServerConfig) -> Result<String> {
    let file = server_file(cwd, project, config_dir);
    let mut servers = load_mcp_file(&file)?;
    servers.insert(name.to_string(), server);
    save_mcp_servers(cwd, config_dir, project, &servers)?;
    Ok(file.display().to_string())
}

pub fn remove_mcp_server(cwd: &Path, config_dir: &Path, project: bool, name: &str) -> Result<bool> {
    let file = server_file(cwd, project, config_dir);
    let mut servers = load_mcp_file(&file)?;
    if servers.remove(name).is_none() {
        return Ok(false);
    }
    save_mcp_servers(cwd, config_dir, project, &servers)?;
    Ok(true)
}

// ---------------------------------------------------------------------------
// OAuth: PKCE + persisted tokens (mirrors the TS McpOAuthProvider)
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthTokens {
    pub access_token: String,
    pub token_type: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_in: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
}

pub struct McpOAuthProvider {
    pub redirect_url: String,
    config: McpOAuthConfig,
    token_file: PathBuf,
    verifier_file: PathBuf,
    server_root: String,
    verifier: Mutex<Option<String>>,
}

impl McpOAuthProvider {
    pub fn new(name: &str, config: McpOAuthConfig, server_url: &str) -> Self {
        let home = crate::config::home_dir();
        let token_file = config
            .token_file
            .clone()
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".pi").join("agent").join("mcp-tokens").join(format!("{name}.json")));
        let verifier_file = token_file.with_extension("verifier");
        Self {
            redirect_url: config.redirect_uri.clone().unwrap_or_else(|| "http://127.0.0.1:8765/callback".into()),
            server_root: server_root(server_url),
            token_file,
            verifier_file,
            config,
            verifier: Mutex::new(None),
        }
    }

    pub fn tokens(&self) -> Option<OAuthTokens> {
        let content = std::fs::read_to_string(&self.token_file).ok()?;
        serde_json::from_str(&content).ok()
    }

    fn write_private(&self, file: &Path, content: &str) -> Result<()> {
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        #[cfg(unix)]
        {
            use std::io::Write;
            use std::os::unix::fs::OpenOptionsExt;
            let mut file = std::fs::OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(file)?;
            file.write_all(content.as_bytes())?;
        }
        #[cfg(not(unix))]
        std::fs::write(file, content)?;
        Ok(())
    }

    pub fn save_tokens(&self, tokens: &OAuthTokens) -> Result<()> {
        self.write_private(&self.token_file, &format!("{}\n", serde_json::to_string_pretty(tokens)?))
    }

    fn code_verifier(&self) -> String {
        let mut verifier = self.verifier.lock().unwrap();
        if verifier.is_none() {
            *verifier = Some(
                std::fs::read_to_string(&self.verifier_file)
                    .ok()
                    .filter(|content| !content.trim().is_empty())
                    .map(|content| content.trim().to_string())
                    .unwrap_or_else(|| format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple())),
            );
        }
        let value = verifier.clone().unwrap_or_default();
        let _ = self.write_private(&self.verifier_file, &value);
        value
    }

    pub fn has_verifier(&self) -> bool {
        self.verifier.lock().unwrap().is_some() || self.verifier_file.exists()
    }

    /// RFC 8414 metadata discovery; falls back to origin defaults.
    async fn discover_metadata(&self) -> Result<(String, Option<String>)> {
        let url = format!("{}/.well-known/oauth-authorization-server", self.server_root);
        let response = reqwest::Client::new().get(&url).header("accept", "application/json").send().await?;
        if response.status().is_success() {
            let metadata: Value = response.json().await?;
            if let Some(authorization) = metadata["authorization_endpoint"].as_str() {
                return Ok((authorization.to_string(), metadata["token_endpoint"].as_str().map(str::to_string)));
            }
        }
        Ok((format!("{}/authorize", self.server_root), Some(format!("{}/token", self.server_root))))
    }

    /// Build the authorization URL and persist the PKCE verifier.
    pub async fn authorization_url(&self) -> Result<String> {
        let verifier = self.code_verifier();
        let challenge = {
            let mut hasher = Sha256::new();
            hasher.update(verifier.as_bytes());
            base64url(&hasher.finalize())
        };
        let (authorization_endpoint, _) = self.discover_metadata().await?;
        Ok(format!(
            "{}?response_type=code&client_id={}&redirect_uri={}&state={}&code_challenge={}&code_challenge_method=S256{}",
            authorization_endpoint,
            urlencode(&self.config.client_id),
            urlencode(&self.redirect_url),
            urlencode(&uuid::Uuid::new_v4().to_string()),
            challenge,
            self.config.scope.as_ref().map(|scope| format!("&scope={}", urlencode(scope))).unwrap_or_default()
        ))
    }

    /// Exchange the authorization code for tokens.
    pub async fn finish_auth(&self, code: &str) -> Result<OAuthTokens> {
        let (_, token_endpoint) = self.discover_metadata().await?;
        let endpoint = token_endpoint.unwrap_or_else(|| format!("{}/token", self.server_root));
        let verifier = self.code_verifier();
        let mut form = vec![
            ("grant_type", "authorization_code".to_string()),
            ("code", code.to_string()),
            ("code_verifier", verifier),
            ("client_id", self.config.client_id.clone()),
            ("redirect_uri", self.redirect_url.clone()),
        ];
        if let Some(secret) = &self.config.client_secret {
            form.push(("client_secret", secret.clone()));
        }
        let response = reqwest::Client::new().post(&endpoint).form(&form).send().await?;
        let status = response.status();
        if !status.is_success() {
            bail!("MCP token exchange failed: HTTP {}", status.as_u16());
        }
        let tokens: OAuthTokens = response.json().await?;
        self.save_tokens(&tokens)?;
        Ok(tokens)
    }
}

fn server_root(url: &str) -> String {
    let _ = url;
    url.to_string()
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut output = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = *chunk.get(1).unwrap_or(&0) as u32;
        let b2 = *chunk.get(2).unwrap_or(&0) as u32;
        let bits = (b0 << 16) | (b1 << 8) | b2;
        output.push(ALPHABET[(bits >> 18) as usize & 63] as char);
        output.push(ALPHABET[(bits >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            output.push(ALPHABET[(bits >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            output.push(ALPHABET[bits as usize & 63] as char);
        }
    }
    output
}

fn urlencode(value: &str) -> String {
    let mut out = String::new();
    for byte in value.as_bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => out.push(*byte as char),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Transports
// ---------------------------------------------------------------------------

type PendingMap = Arc<Mutex<HashMap<u64, tokio::sync::oneshot::Sender<Value>>>>;
type Writer = tokio::sync::mpsc::UnboundedSender<String>;

struct StdioRuntime {
    child: tokio::process::Child,
    writer: Writer,
}

struct HttpRuntime {
    url: String,
    headers: Arc<Vec<(String, String)>>,
    session_id: Mutex<Option<String>>,
    oauth: Option<Arc<tokio::sync::Mutex<McpOAuthProvider>>>,
}

enum Transport {
    Stdio(StdioRuntime),
    Http(Box<HttpRuntime>),
}

pub struct McpClient {
    next_id: AtomicU64,
    pending: PendingMap,
    transport: Transport,
    interactions: Option<Arc<dyn McpInteractions>>,
}

fn json_rpc(id: u64, method: &str, params: &Value) -> String {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string()
}

impl McpClient {
    pub async fn connect(config: &McpServerConfig, cwd: &Path, interactions: Option<Arc<dyn McpInteractions>>) -> Result<Arc<Self>> {
        let next_id = AtomicU64::new(0);
        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (transport, writer) = Self::open_transport(config, cwd, interactions.clone(), pending.clone()).await?;
        let client = Arc::new(Self { next_id, pending, transport, interactions });
        let _ = writer; // stdio writer already wired into the reader
        client.initialize_with_writer(&writer).await?;
        Ok(client)
    }

    async fn open_transport(config: &McpServerConfig, cwd: &Path, interactions: Option<Arc<dyn McpInteractions>>, pending: PendingMap) -> Result<(Transport, Writer)> {
        let (notify_sender, _notify_receiver) = tokio::sync::mpsc::unbounded_channel();
        if let Some(command) = &config.command {
            let mut process = tokio::process::Command::new(command);
            process
                .args(config.args.clone().unwrap_or_default())
                .current_dir(cwd)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null());
            for (key, value) in std::env::vars() {
                process.env(&key, &value);
            }
            for (key, value) in config.env.clone().unwrap_or_default() {
                process.env(&key, &value);
            }
            let mut child = process.spawn()?;
            let stdin = child.stdin.take().ok_or_else(|| anyhow!("MCP server stdin unavailable"))?;
            let stdout = child.stdout.take().ok_or_else(|| anyhow!("MCP server stdout unavailable"))?;
            let (writer, writer_receiver) = tokio::sync::mpsc::unbounded_channel::<String>();
            spawn_stdio_writer(stdin, writer_receiver);
            spawn_stdio_reader(stdout, pending, interactions, notify_sender, writer.clone());
            Ok((Transport::Stdio(StdioRuntime { child, writer: writer.clone() }), writer))
        } else if let Some(url) = &config.url {
            let oauth = config.oauth.clone().map(|oauth| Arc::new(tokio::sync::Mutex::new(McpOAuthProvider::new("server", oauth, url))));
            let headers: Arc<Vec<(String, String)>> = Arc::new(config.headers.clone().unwrap_or_default().into_iter().collect());
            let runtime = HttpRuntime { url: url.clone(), headers, session_id: Mutex::new(None), oauth };
            Ok((Transport::Http(Box::new(runtime)), writer_none()))
        } else {
            bail!("MCP server requires command or url");
        }
    }

    async fn initialize_with_writer(&self, _writer: &Writer) -> Result<Value> {
        let response = self
            .request("initialize", json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": {"name": CLIENT_NAME, "version": CLIENT_VERSION},
            }))
            .await?;
        match &self.transport {
            Transport::Stdio(runtime) => {
                let _ = runtime.writer.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}).to_string());
            }
            Transport::Http(_) => {
                let _ = self.post_raw_inner(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}), false).await;
            }
        }
        Ok(response)
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        let (sender, receiver) = tokio::sync::oneshot::channel();
        self.pending.lock().unwrap().insert(id, sender);
        let payload = json_rpc(id, method, &params);
        let sent = match &self.transport {
            Transport::Stdio(runtime) => runtime.writer.send(payload).is_ok(),
            Transport::Http(_) => self.post_raw_inner(serde_json::from_str::<Value>(&payload)?, false).await.is_ok(),
        };
        if !sent {
            self.pending.lock().unwrap().remove(&id);
            bail!("MCP transport failed to send: {method}");
        }
        let timeout_ms = std::time::Duration::from_secs(120);
        match tokio::time::timeout(timeout_ms, receiver).await {
            Ok(Ok(response)) => {
                if response["error"].is_object() {
                    bail!("{}", response["error"]["message"].as_str().unwrap_or("MCP request failed"));
                }
                Ok(response["result"].clone())
            }
            _ => bail!("MCP request failed or timed out: {method}"),
        }
    }

    /// POST a JSON-RPC request and resolve its response inline (HTTP transports).
    async fn post_raw_value(&self, payload: Value) -> Result<Option<Value>> {
        self.post_raw_inner(payload, true).await
    }

    async fn post_raw_inner(&self, payload: Value, expect_response: bool) -> Result<Option<Value>> {
        let Transport::Http(runtime) = &self.transport else { bail!("not an HTTP transport") };
        let mut request = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .unwrap_or_default()
            .post(&runtime.url)
            .header("content-type", "application/json")
            .header("accept", "application/json, text/event-stream");
        for (name, value) in runtime.headers.iter() {
            request = request.header(name.as_str(), value.as_str());
        }
        if let Some(session_id) = runtime.session_id.lock().unwrap().clone() {
            request = request.header("mcp-session-id", session_id);
        }
        if let Some(oauth) = &runtime.oauth {
            if let Some(tokens) = oauth.lock().await.tokens() {
                request = request.header("authorization", format!("Bearer {}", tokens.access_token));
            }
        }
        let response = request.body(serde_json::to_string(&payload)?).send().await?;
        let status = response.status();
        if let Some(session) = response.headers().get("mcp-session-id").and_then(|value| value.to_str().ok()).map(str::to_string) {
            *runtime.session_id.lock().unwrap() = Some(session);
        }
        if status == reqwest::StatusCode::UNAUTHORIZED {
            if let Some(oauth) = &runtime.oauth {
                let provider = oauth.lock().await;
                let url = provider.authorization_url().await?;
                bail!("MCP OAuth authorization required: {url}");
            }
            bail!("MCP request unauthorized: HTTP 401");
        }
        if status.as_u16() == 202 || status.as_u16() == 204 {
            return Ok(None);
        }
        if !status.is_success() {
            bail!("MCP HTTP error: status {}", status.as_u16());
        }
        let content_type = response.headers().get("content-type").and_then(|value| value.to_str().ok()).unwrap_or_default().to_string();
        let body = response.text().await?;
        if content_type.contains("text/event-stream") {
            for line in body.split('\n') {
                let trimmed = line.trim();
                if let Some(data) = trimmed.strip_prefix("data:") {
                    if let Ok(value) = serde_json::from_str::<Value>(data.trim()) {
                        if value["id"].is_u64() {
                            return Ok(Some(value));
                        }
                    }
                }
            }
            Ok(None)
        } else {
            let parsed: Value = serde_json::from_str(&body).map_err(|error| anyhow!("MCP HTTP response not JSON: {error}"))?;
            Ok(expect_response.then_some(parsed))
        }
    }

    pub async fn list_tools(&self) -> Result<Value> {
        self.request("tools/list", json!({})).await
    }

    pub async fn call_tool(&self, name: &str, arguments: &Value) -> Result<Value> {
        self.request("tools/call", json!({"name": name, "arguments": arguments})).await
    }

    pub async fn list_resources(&self) -> Result<Value> {
        self.request("resources/list", json!({})).await
    }

    pub async fn read_resource(&self, uri: &str) -> Result<Value> {
        self.request("resources/read", json!({"uri": uri})).await
    }

    pub async fn list_prompts(&self) -> Result<Value> {
        self.request("prompts/list", json!({})).await
    }

    pub async fn get_prompt(&self, name: &str, prompt_arguments: &Value) -> Result<Value> {
        self.request("prompts/get", json!({"name": name, "arguments": prompt_arguments})).await
    }

    pub async fn close(&self) {
        let child = match &self.transport {
            Transport::Stdio(runtime) => Some(&runtime.child),
            Transport::Http(_) => None,
        };
        // start_kill needs &mut; the Child is stored behind & for async runner semantics.
        if let Some(mut child) = child {
            let _ = child;
        }
        if let Transport::Stdio(runtime) = &self.transport {
            let pid = runtime.child.id();
            if let Some(pid) = pid {
                unsafe {
                    kill_c(pid as i32, 15);
                }
            }
        }
    }
}

extern "C" {
    #[link_name = "kill"]
    fn kill_c(pid: i32, signal: i32) -> i32;
}

fn writer_none() -> Writer {
    // For HTTP transports pending responses arrive in-band; still keep shape.
    let (sender, mut receiver) = tokio::sync::mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while receiver.recv().await.is_some() {} // drain
    });
    sender
}

/// Writer task: serializes outbound JSON-RPC lines to the child's stdin.
fn spawn_stdio_writer(mut stdin: tokio::process::ChildStdin, mut receiver: tokio::sync::mpsc::UnboundedReceiver<String>) {
    tokio::spawn(async move {
        use tokio::io::AsyncWriteExt;
        while let Some(line) = receiver.recv().await {
            if stdin.write_all(format!("{line}\n").as_bytes()).await.is_err() {
                break;
            }
            if stdin.flush().await.is_err() {
                break;
            }
        }
    });
}

/// Reader task for stdio: resolves pending responses, serves server→client
/// requests (elicitation/sampling via the interaction surface), and forwards
/// notifications to the manager channel.
fn spawn_stdio_reader<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
    stream: R,
    pending: PendingMap,
    interactions: Option<Arc<dyn McpInteractions>>,
    notify_sender: tokio::sync::mpsc::UnboundedSender<Value>,
    writer: Writer,
) {
    tokio::spawn(async move {
        let mut reader = tokio::io::BufReader::new(stream);
        let mut line = String::new();
        use tokio::io::AsyncBufReadExt;
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let Ok(message) = serde_json::from_str::<Value>(line.trim()) else { continue };
            if let Some(id) = message["id"].as_u64() {
                if message["method"].is_string() {
                    // server→client request: reply over stdin
                    let reply = handle_server_request(&message, &interactions).await;
                    let _ = reply;
                    let _ = writer.send(json!({"jsonrpc": "2.0", "id": id, "result": reply}).to_string());
                    continue;
                }
                if let Some(sender) = pending.lock().unwrap().remove(&id) {
                    let _ = sender.send(message);
                }
                continue;
            }
            if let Some(method) = message["method"].as_str() {
                let _ = notify_sender.send(json!({"method": method, "params": message["params"].clone()}));
            }
        }
        pending.lock().unwrap().clear();
    });
}

async fn handle_server_request(message: &Value, interactions: &Option<Arc<dyn McpInteractions>>) -> Value {
    let Some(method) = message["method"].as_str() else { return json!({"action": "decline"}) };
    let params = message["params"].clone();
    match (method, interactions) {
        ("elicitation/create", Some(interactions)) => interactions.elicit(params).await.unwrap_or_else(|_| json!({"action": "decline"})),
        ("sampling/createMessage", Some(interactions)) => interactions.sample(params).await.unwrap_or_else(|_| json!({"action": "decline"})),
        _ => json!({"action": "decline"}),
    }
}

// ---------------------------------------------------------------------------
// Manager
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct McpToolEntry {
    pub server: String,
    pub remote_name: String,
    pub declaration: &'static ToolDeclaration,
    pub client: Arc<McpClient>,
}

pub struct McpManager {
    pub servers: BTreeMap<String, McpServerConfig>,
    cwd: PathBuf,
    active: Mutex<BTreeMap<String, Arc<McpClient>>>,
    tools: Arc<Mutex<BTreeMap<String, McpToolEntry>>>,
    interactions: Option<Arc<dyn McpInteractions>>,
    pending_auth: Mutex<std::collections::HashSet<String>>,
}

pub type ApproveFn = dyn Fn(crate::security::permissions::OperationRequest) -> std::pin::Pin<Box<dyn std::future::Future<Output = crate::security::permissions::ApprovalResult> + Send>> + Send + Sync;

impl McpManager {
    pub fn new(cwd: &Path, servers: BTreeMap<String, McpServerConfig>, interactions: Option<Arc<dyn McpInteractions>>) -> Self {
        Self { servers, cwd: cwd.to_path_buf(), active: Mutex::new(BTreeMap::new()), tools: Arc::new(Mutex::new(BTreeMap::new())), interactions, pending_auth: Mutex::new(std::collections::HashSet::new()) }
    }

    pub fn names(&self) -> Vec<String> {
        self.servers.iter().filter(|(_, config)| config.enabled != Some(false)).map(|(name, _)| name.clone()).collect()
    }

    pub async fn connect(&self, name: &str, approve: Option<&ApproveFn>) -> Result<Arc<McpClient>> {
        if let Some(active) = self.active.lock().unwrap().get(name) {
            return Ok(active.clone());
        }
        let config = self.servers.get(name).cloned().ok_or_else(|| anyhow!("Unknown or disabled MCP server: {name}"))?;
        if config.enabled == Some(false) {
            bail!("Unknown or disabled MCP server: {name}");
        }
        if config.command.is_none() && config.url.is_none() {
            bail!("MCP server {name} requires command or url");
        }
        if let Some(approve) = approve {
            let request = crate::security::permissions::OperationRequest {
                tool: format!("mcp:{name}"),
                risk: crate::types::RiskLevel::Mcp,
                arguments: json!({"server": name, "target": config.command.clone().or(config.url.clone()).unwrap_or_default()}),
                summary: format!("Connect to MCP server {name}"),
            };
            let approval = approve(request).await;
            if approval.decision != "allow" {
                bail!("MCP connection denied: {name}");
            }
        }
        let client = McpClient::connect(&config, &self.cwd, self.interactions.clone()).await?;
        let tools_result = client.list_tools().await;
        if let Err(error) = tools_result {
            let message = error.to_string();
            client.close().await;
            if message.contains("MCP OAuth authorization required") {
                let oauth_config = config.oauth.clone().ok_or_else(|| anyhow!("MCP OAuth authorization required for {name} but no oauth configuration is present"))?;
                let url = McpOAuthProvider::new(name, oauth_config, config.url.as_deref().unwrap_or_default()).authorization_url().await?;
                self.pending_auth.lock().unwrap().insert(name.to_string());
                bail!("MCP OAuth authorization required for {name}: {url}");
            }
            bail!("MCP tools/list failed for {name}: {message}");
        }
        self.load_tools(name, client.clone()).await?;
        self.active.lock().unwrap().insert(name.to_string(), client.clone());
        Ok(client)
    }

    async fn load_tools(&self, name: &str, client: Arc<McpClient>) -> Result<()> {
        let listed = client.list_tools().await.map_err(|error| anyhow!("MCP tools/list failed: {error}"))?;
        let mut tools = self.tools.lock().unwrap();
        let stale: Vec<String> = tools.iter().filter(|(_, entry)| entry.server == name).map(|(key, _)| key.clone()).collect();
        for key in stale {
            tools.remove(&key);
        }
        for remote in listed["tools"].as_array().cloned().unwrap_or_default() {
            let remote_name = remote["name"].as_str().unwrap_or_default().to_string();
            if remote_name.is_empty() {
                continue;
            }
            let local_name = format!("mcp__{}__{}", sanitize_mcp_name(name), sanitize_mcp_name(&remote_name));
            let description = format!("[{name}] {}", remote["description"].as_str().unwrap_or(&remote_name));
            let declaration: &'static ToolDeclaration = Box::leak(Box::new(ToolDeclaration { name: local_name.clone(), description, parameters: remote["inputSchema"].clone() }));
            tools.insert(
                local_name.clone(),
                McpToolEntry { server: name.to_string(), remote_name, declaration, client: client.clone() },
            );
        }
        Ok(())
    }

    pub async fn connect_selected(&self, names: &[String], approve: Option<&ApproveFn>) -> Result<()> {
        for name in names {
            self.connect(name, approve).await?;
        }
        Ok(())
    }

    pub async fn finish_auth(&self, name: &str, code: &str) -> Result<()> {
        if !self.pending_auth.lock().unwrap().contains(name) {
            bail!("No pending MCP authorization for {name}");
        }
        let config = self.servers.get(name).cloned().ok_or_else(|| anyhow!("No pending MCP authorization for {name}"))?;
        let oauth_config = config.oauth.clone().ok_or_else(|| anyhow!("No pending MCP authorization for {name}"))?;
        let provider = McpOAuthProvider::new(name, oauth_config, config.url.as_deref().unwrap_or_default());
        provider.finish_auth(code).await?;
        self.pending_auth.lock().unwrap().remove(name);
        Ok(())
    }

    pub async fn refresh_tools(&self, name: &str) -> Result<()> {
        let client = self.active.lock().unwrap().get(name).cloned();
        let Some(client) = client else { bail!("MCP server is not connected: {name}") };
        self.load_tools(name, client).await
    }

    pub async fn list_resources(&self, name: &str) -> Result<Value> {
        self.client_for(name)?.list_resources().await
    }

    pub async fn read_resource(&self, name: &str, uri: &str) -> Result<Value> {
        self.client_for(name)?.read_resource(uri).await
    }

    pub async fn list_prompts(&self, name: &str) -> Result<Value> {
        self.client_for(name)?.list_prompts().await
    }

    pub async fn get_prompt(&self, name: &str, prompt_name: &str, prompt_arguments: &Value) -> Result<Value> {
        self.client_for(name)?.get_prompt(prompt_name, prompt_arguments).await
    }

    pub fn tool_names(&self) -> Vec<String> {
        self.tools.lock().unwrap().keys().cloned().collect()
    }

    pub fn get_tool(&self, name: &str) -> Option<McpToolEntry> {
        self.tools.lock().unwrap().get(name).cloned()
    }

    pub fn active_names(&self) -> Vec<String> {
        self.active.lock().unwrap().keys().cloned().collect()
    }

    pub async fn close(&self) {
        for client in self.active.lock().unwrap().values() {
            client.close().await;
        }
        self.active.lock().unwrap().clear();
        self.tools.lock().unwrap().clear();
        self.pending_auth.lock().unwrap().clear();
    }

    fn client_for(&self, name: &str) -> Result<Arc<McpClient>> {
        self.active.lock().unwrap().get(name).cloned().ok_or_else(|| anyhow!("MCP server is not connected: {name}"))
    }
}

pub fn format_mcp_result(content: &Value) -> String {
    let formatted = if let Some(items) = content.as_array() {
        items
            .iter()
            .filter_map(|item| match item["type"].as_str() {
                Some("text") => Some(item["text"].as_str().unwrap_or_default().to_string()),
                Some("image") => Some(format!("[image {}]", item["mimeType"].as_str().unwrap_or("unknown"))),
                Some("resource") => Some(serde_json::to_string(&item["resource"]).unwrap_or_default()),
                Some(_) => Some(serde_json::to_string(item).unwrap_or_default()),
                None => item.as_str().map(str::to_string),
            })
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("\n")
    } else {
        serde_json::to_string(content).unwrap_or_default()
    };
    let clipped: String = formatted.chars().take(100_000).collect();
    if clipped.is_empty() { "(no output)".to_string() } else { clipped }
}

/// Wrapper exposing MCP tools to the agent loop.
pub struct McpTool {
    pub entry: McpToolEntry,
}

#[async_trait::async_trait]
impl crate::tools::Tool for McpTool {
    fn declaration(&self) -> &ToolDeclaration {
        self.entry.declaration
    }

    fn risk(&self) -> crate::types::RiskLevel {
        crate::types::RiskLevel::Mcp
    }

    async fn execute(&self, args: &Value) -> Result<String> {
        let result = self.entry.client.call_tool(&self.entry.remote_name, args).await?;
        let is_error = result["isError"].as_bool().unwrap_or(false);
        let content = format_mcp_result(&result["content"]);
        if is_error {
            return Err(anyhow!(content));
        }
        let structured = result["structuredContent"].is_object().then(|| serde_json::to_string(&result["structuredContent"]).unwrap_or_default()).unwrap_or_default();
        Ok([content, structured].iter().filter(|part| !part.is_empty()).cloned().collect::<Vec<_>>().join("\n"))
    }
}
