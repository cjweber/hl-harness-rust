//! Stateful runtime tools ported from `src/tools/index.ts`: background jobs,
//! persistent shells, filesystem watches, and the sandboxed shell.

use super::{clip, run_process, Tool, OUTPUT_LIMIT};
use crate::types::{RiskLevel, ToolDeclaration};
use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

extern "C" {
    #[link_name = "kill"]
    fn kill_c(pid: i32, signal: i32) -> i32;
}

fn declaration_once(name: &str, description: &str, parameters: serde_json::Value) -> ToolDeclaration {
    ToolDeclaration { name: name.to_string(), description: description.to_string(), parameters }
}

// ---------------------------------------------------------------------------
// Background jobs
// ---------------------------------------------------------------------------

pub struct Job {
    pid: Option<u32>,
    output: Arc<Mutex<String>>,
    done: Arc<AtomicBool>,
    code: Arc<Mutex<Option<i32>>>,
}

#[derive(Default)]
pub struct JobRegistry {
    next_id: AtomicU64,
    jobs: Mutex<HashMap<String, Job>>,
}

impl JobRegistry {
    pub fn start(&self, command: &str, cwd: &Path) -> Result<String> {
        let id = format!("job_{}", self.next_id.fetch_add(1, Ordering::SeqCst) + 1);
        let mut command_child = tokio::process::Command::new("/bin/bash");
        command_child
            .args(["-lc", command])
            .current_dir(cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null());
        let mut child = command_child.spawn()?;
        let output = Arc::new(Mutex::new(String::new()));
        let done = Arc::new(AtomicBool::new(false));
        let code = Arc::new(Mutex::new(None));
        let pid = child.id();
        if let Some(stdout) = child.stdout.take() {
            spawn_bounded_reader(stdout, output.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_bounded_reader(stderr, output.clone());
        }
        let wait_code = code.clone();
        let wait_done = done.clone();
        tokio::spawn(async move {
            if let Ok(status) = child.wait().await {
                *wait_code.lock().unwrap() = status.code();
            }
            wait_done.store(true, Ordering::SeqCst);
        });
        self.jobs.lock().unwrap().insert(id.clone(), Job { pid, output, done, code });
        Ok(id)
    }

    fn status(&self, id: &str) -> Result<String> {
        let jobs = self.jobs.lock().unwrap();
        let job = jobs.get(id).ok_or_else(|| anyhow!("Unknown background job"))?;
        Ok(format!(
            "status={} code={}\n{}",
            if job.done.load(Ordering::SeqCst) { "done" } else { "running" },
            job.code.lock().unwrap().map(|code| code.to_string()).unwrap_or_else(|| "-".into()),
            if job.output.lock().unwrap().is_empty() { "(no output)".to_string() } else { clip(&job.output.lock().unwrap(), OUTPUT_LIMIT) }
        ))
    }

    fn cancel(&self, id: &str) -> Result<String> {
        let jobs = self.jobs.lock().unwrap();
        let job = jobs.get(id).ok_or_else(|| anyhow!("Unknown background job"))?;
        if !job.done.load(Ordering::SeqCst) {
            if let Some(pid) = job.pid {
                unsafe {
                    kill_c(pid as i32, 15);
                }
            }
        }
        Ok(format!("Cancelled background job {id}"))
    }
}

fn spawn_bounded_reader<R: tokio::io::AsyncRead + Unpin + Send + 'static>(mut stream: R, output: Arc<Mutex<String>>) {
    tokio::spawn(async move {
        let mut buffer = [0u8; 4096];
        loop {
            match tokio::io::AsyncReadExt::read(&mut stream, &mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => {
                    let mut guard = output.lock().unwrap();
                    if guard.len() < 20_000 {
                        let text = String::from_utf8_lossy(&buffer[..read]).to_string();
                        let budget = 20_000 - guard.len();
                        let mut end = text.len().min(budget);
                        while end > 0 && !text.is_char_boundary(end) {
                            end -= 1;
                        }
                        guard.push_str(&text[..end]);
                    }
                }
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Persistent shells
// ---------------------------------------------------------------------------

pub struct ShellSession {
    stdin: Arc<Mutex<Option<tokio::process::ChildStdin>>>,
    inbox: Arc<Mutex<String>>,
    running: Arc<AtomicBool>,
    pub exited: Arc<AtomicBool>,
}

#[derive(Default)]
pub struct ShellRegistry {
    next_id: AtomicU64,
    shells: Mutex<HashMap<String, Arc<ShellSession>>>,
}

impl ShellRegistry {
    pub fn get_or_create(&self, id: Option<&str>, cwd: &Path) -> Result<(String, Arc<ShellSession>)> {
        if let Some(id) = id {
            let shells = self.shells.lock().unwrap();
            return match shells.get(id) {
                Some(shell) => Ok((id.to_string(), shell.clone())),
                None => bail!("Unknown persistent shell session"),
            };
        }
        let id = format!("shell_{}", self.next_id.fetch_add(1, Ordering::SeqCst) + 1);
        let mut command = tokio::process::Command::new("/bin/bash");
        command.args(["--noprofile", "--norc"]).current_dir(cwd).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).stdin(std::process::Stdio::piped());
        let mut child = command.spawn()?;
        let stdin = Arc::new(Mutex::new(child.stdin.take()));
        let inbox = Arc::new(Mutex::new(String::new()));
        let exited = Arc::new(AtomicBool::new(false));
        if let Some(stdout) = child.stdout.take() {
            spawn_inbox_reader(stdout, inbox.clone());
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_inbox_reader(stderr, inbox.clone());
        }
        let session = Arc::new(ShellSession { stdin, inbox, running: Arc::new(AtomicBool::new(false)), exited: exited.clone() });
        tokio::spawn(async move {
            let _ = child.wait().await;
            exited.store(true, Ordering::SeqCst);
        });
        self.shells.lock().unwrap().insert(id.clone(), session.clone());
        Ok((id, session))
    }

    pub async fn run(&self, session: &ShellSession, command: &str) -> Result<String> {
        if session.running.load(Ordering::SeqCst) {
            bail!("Persistent shell is already running a command");
        }
        session.running.store(true, Ordering::SeqCst);
        let result = self.send_and_collect(session, command).await;
        session.running.store(false, Ordering::SeqCst);
        result
    }

    async fn send_and_collect(&self, session: &ShellSession, command: &str) -> Result<String> {
        let marker = format!("__HARNESS_SHELL_{}__", uuid::Uuid::new_v4().to_string().replace('-', ""));
        session.inbox.lock().unwrap().clear();
        let payload = format!("{command}\nprintf '\\n%s\\n' '{marker}'\n");
        let write_fut = {
            let mut guard = session.stdin.lock().unwrap();
            let Some(mut stdin) = guard.take() else { bail!("Persistent shell stdin closed") };
            async move {
                use tokio::io::AsyncWriteExt;
                let _ = stdin.write_all(payload.as_bytes()).await;
                let _ = stdin.flush().await;
                stdin
            }
        };
        let stdin = write_fut.await;
        *session.stdin.lock().unwrap() = Some(stdin);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        loop {
            {
                let guard = session.inbox.lock().unwrap();
                if let Some(index) = guard.find(&marker) {
                    let output = guard[..index].trim_end_matches('\n').to_string();
                    return Ok(clip(&output, OUTPUT_LIMIT));
                }
                if session.exited.load(Ordering::SeqCst) {
                    return Ok(clip(&format!("{}\n[shell exited]", guard.clone()), OUTPUT_LIMIT));
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return Ok(clip("\n[shell command timed out]", OUTPUT_LIMIT));
            }
            tokio::time::sleep(Duration::from_millis(40)).await;
        }
    }

    pub async fn close(&self, id: &str) -> Result<String> {
        let session = {
            let mut shells = self.shells.lock().unwrap();
            shells.remove(id).ok_or_else(|| anyhow!("Unknown persistent shell session"))?
        };
        let stdin = {
            let mut guard = session.stdin.lock().unwrap();
            guard.take()
        };
        if let Some(mut stdin) = stdin {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.shutdown().await;
        }
        Ok(format!("Closed persistent shell session {id}"))
    }
}

fn spawn_inbox_reader<R: tokio::io::AsyncRead + Unpin + Send + 'static>(mut stream: R, inbox: Arc<Mutex<String>>) {
    tokio::spawn(async move {
        let mut buffer = [0u8; 4096];
        loop {
            match tokio::io::AsyncReadExt::read(&mut stream, &mut buffer).await {
                Ok(0) | Err(_) => break,
                Ok(read) => inbox.lock().unwrap().push_str(&String::from_utf8_lossy(&buffer[..read])),
            }
        }
    });
}

// ---------------------------------------------------------------------------
// Filesystem watches (notify)
// ---------------------------------------------------------------------------

#[derive(Default)]
pub struct WatchRegistry {
    next_id: AtomicU64,
    watches: Mutex<HashMap<String, WatchHandle>>,
}

pub struct WatchHandle {
    _watcher: notify::RecommendedWatcher,
    changes: Arc<Mutex<Vec<String>>>,
}

impl WatchRegistry {
    pub fn start(&self, root: &Path, display: &str) -> Result<String> {
        let id = format!("watch_{}", self.next_id.fetch_add(1, Ordering::SeqCst) + 1);
        let changes = Arc::new(Mutex::new(Vec::new()));
        let (sender, receiver) = std::sync::mpsc::channel();
        let mut watcher = notify::recommended_watcher(sender)?;
        notify::Watcher::watch(&mut watcher, root, notify::RecursiveMode::Recursive)?;
        let drain_changes = changes.clone();
        std::thread::spawn(move || {
            for event in receiver {
                if let Ok(event) = event {
                    let kind = format!("{:?}", event.kind);
                    let target = event.paths.first().map(|path| path.to_string_lossy().to_string()).unwrap_or_default();
                    let mut guard = drain_changes.lock().unwrap();
                    guard.push(if target.is_empty() { kind } else { format!("{kind} {target}") });
                    if guard.len() > 100 {
                        guard.remove(0);
                    }
                }
            }
        });
        self.watches.lock().unwrap().insert(id.clone(), WatchHandle { _watcher: watcher, changes });
        Ok(format!("watch={id} target={display}"))
    }

    pub fn read_and_clear(&self, id: &str) -> Result<String> {
        let watches = self.watches.lock().unwrap();
        let handle = watches.get(id).ok_or_else(|| anyhow!("Unknown filesystem watch"))?;
        let drained: Vec<String> = handle.changes.lock().unwrap().drain(..).collect();
        Ok(if drained.is_empty() { "(no changes)".to_string() } else { drained.join("\n") })
    }

    pub fn stop(&self, id: &str) -> Result<String> {
        let mut watches = self.watches.lock().unwrap();
        if watches.remove(id).is_none() {
            bail!("Unknown filesystem watch");
        }
        Ok(format!("Stopped filesystem watch {id}"))
    }
}

// ---------------------------------------------------------------------------
// Tool wrappers
// ---------------------------------------------------------------------------

pub struct BashBackgroundTool {
    cwd: PathBuf,
    jobs: Arc<JobRegistry>,
}

#[async_trait]
impl Tool for BashBackgroundTool {
     fn declaration(&self) -> &ToolDeclaration {
        let owned = declaration_once("bash_background", "Start a bounded-output background shell command in the project directory.", json!({"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"], "additionalProperties": false}));
        Box::leak(Box::new(owned))
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Shell
    }

    async fn execute(&self, args: &Value) -> Result<String> {
        let command = args.get("command").and_then(Value::as_str).unwrap_or_default().trim().to_string();
        if command.is_empty() {
            bail!("bash_background requires a command");
        }
        Ok(format!("Started background job {}", self.jobs.start(&command, &self.cwd)?))
    }
}

pub struct BashJobTool {
    jobs: Arc<JobRegistry>,
}

#[async_trait]
impl Tool for BashJobTool {
     fn declaration(&self) -> &ToolDeclaration {
        let owned = declaration_once("bash_job", "Read status and bounded output from a background shell job.", json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"], "additionalProperties": false}));
        Box::leak(Box::new(owned))
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Safe
    }

    async fn execute(&self, args: &Value) -> Result<String> {
        self.jobs.status(args.get("id").and_then(Value::as_str).unwrap_or_default())
    }
}

pub struct BashCancelTool {
    jobs: Arc<JobRegistry>,
}

#[async_trait]
impl Tool for BashCancelTool {
     fn declaration(&self) -> &ToolDeclaration {
        let owned = declaration_once("bash_cancel", "Terminate a running background shell job.", json!({"type": "object", "properties": {"id": {"type": "string"}}, "required": ["id"], "additionalProperties": false}));
        Box::leak(Box::new(owned))
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Shell
    }

    async fn execute(&self, args: &Value) -> Result<String> {
        self.jobs.cancel(args.get("id").and_then(Value::as_str).unwrap_or_default())
    }
}

pub struct BashSessionTool {
    cwd: PathBuf,
    shells: Arc<ShellRegistry>,
}

#[async_trait]
impl Tool for BashSessionTool {
     fn declaration(&self) -> &ToolDeclaration {
        let owned = declaration_once("bash_session", "Run a command in a persistent project shell, creating a session when sessionId is omitted.", json!({"type": "object", "properties": {"command": {"type": "string"}, "sessionId": {"type": "string"}}, "required": ["command"], "additionalProperties": false}));
        Box::leak(Box::new(owned))
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Shell
    }

    async fn execute(&self, args: &Value) -> Result<String> {
        let command = args.get("command").and_then(Value::as_str).unwrap_or_default().trim().to_string();
        if command.is_empty() {
            bail!("bash_session requires a command");
        }
        let (id, shell) = self.shells.get_or_create(args.get("sessionId").and_then(Value::as_str), &self.cwd)?;
        let output = ShellRegistry::run(&self.shells, &shell, &command).await?;
        Ok(format!("session={id}\n{output}"))
    }
}

pub struct BashSessionCloseTool {
    shells: Arc<ShellRegistry>,
}

#[async_trait]
impl Tool for BashSessionCloseTool {
     fn declaration(&self) -> &ToolDeclaration {
        let owned = declaration_once("bash_session_close", "Close a persistent project shell session.", json!({"type": "object", "properties": {"sessionId": {"type": "string"}}, "required": ["sessionId"], "additionalProperties": false}));
        Box::leak(Box::new(owned))
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Shell
    }

    async fn execute(&self, args: &Value) -> Result<String> {
        self.shells.close(args.get("sessionId").and_then(Value::as_str).unwrap_or_default()).await
    }
}

pub struct BashSandboxTool {
    cwd: PathBuf,
}

#[async_trait]
impl Tool for BashSandboxTool {
    fn declaration(&self) -> &ToolDeclaration {
        Box::leak(Box::new(declaration_once("bash_sandbox", "Run a shell command under the macOS sandbox-exec or Linux bubblewrap policy; rejects writes outside the project and is unavailable on unsupported hosts.", json!({"type": "object", "properties": {"command": {"type": "string"}, "timeoutMs": {"type": "integer", "minimum": 100, "maximum": 300000}}, "required": ["command"], "additionalProperties": false}))))
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Shell
    }

    async fn execute(&self, args: &Value) -> Result<String> {
        let command = args.get("command").and_then(Value::as_str).unwrap_or_default().to_string();
        let timeout_ms = args.get("timeoutMs").and_then(Value::as_u64).unwrap_or(120_000).clamp(100, 300_000);
        if command.is_empty() {
            bail!("bash_sandbox requires a command");
        }
        let (executable, mut prefix_args): (String, Vec<String>) = match std::env::consts::OS {
            "macos" => {
                if !Path::new("/usr/bin/sandbox-exec").exists() {
                    bail!("bash_sandbox requires macOS sandbox-exec");
                }
                let escaped = self.cwd.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"");
                ("/usr/bin/sandbox-exec".to_string(), vec!["-p".into(), format!("(version 1)(allow default)(deny file-write*)(allow file-write* (subpath \"{escaped}\"))")])
            }
            "linux" => {
                let requested = std::env::var("HARNESS_SANDBOX_BWRAP").unwrap_or_else(|_| "bwrap".into());
                let executable = executable_on_path(&requested).ok_or_else(|| anyhow!("bash_sandbox requires bubblewrap (bwrap) or HARNESS_SANDBOX_BWRAP"))?;
                let mut prefix = vec![
                    "--die-with-parent".into(),
                    "--new-session".into(),
                    "--unshare-user".into(),
                    "--unshare-pid".into(),
                    "--unshare-ipc".into(),
                    "--unshare-uts".into(),
                    "--unshare-net".into(),
                    "--proc".into(),
                    "/proc".into(),
                    "--dev".into(),
                    "/dev".into(),
                    "--ro-bind".into(),
                    "/usr".into(),
                    "/usr".into(),
                    "--ro-bind".into(),
                    "/bin".into(),
                    "/bin".into(),
                    "--ro-bind".into(),
                    "/lib".into(),
                    "/lib".into(),
                    "--tmpfs".into(),
                    "/tmp".into(),
                    "--bind".into(),
                    self.cwd.to_string_lossy().to_string(),
                    self.cwd.to_string_lossy().to_string(),
                    "--chdir".into(),
                    self.cwd.to_string_lossy().to_string(),
                    "--".into(),
                ];
                for directory in ["/lib64", "/etc"] {
                    if Path::new(directory).exists() {
                        prefix.extend(["--ro-bind".to_string(), directory.to_string(), directory.to_string()]);
                    }
                }
                (executable.to_string_lossy().to_string(), prefix)
            }
            _ => bail!("bash_sandbox supports macOS and Linux hosts only"),
        };
        prefix_args.extend(["/bin/bash".to_string(), "-lc".to_string(), command]);
        let refs: Vec<&str> = prefix_args.iter().map(String::as_str).collect();
        let (code, output) = run_process(&executable, &refs, &self.cwd, None, Some(timeout_ms)).await?;
        let suffix = match code {
            Some(0) => String::new(),
            other => format!("\n[exit {}]", other.map(|code| code.to_string()).unwrap_or_else(|| "unknown".into())),
        };
        Ok(clip(&format!("{output}{suffix}"), OUTPUT_LIMIT))
    }
}

pub fn executable_on_path(name: &str) -> Option<PathBuf> {
    if name.contains('/') {
        return if Path::new(name).exists() { Some(PathBuf::from(name)) } else { None };
    }
    std::env::var("PATH").ok()?.split(':').find_map(|directory| {
        let candidate = Path::new(directory).join(name);
        candidate.exists().then_some(candidate)
    })
}

/// Registry bundle shared by all stateful tool instances.
#[derive(Default)]
pub struct Runtime {
    pub jobs: Arc<JobRegistry>,
    pub shells: Arc<ShellRegistry>,
    pub watches: Arc<WatchRegistry>,
    pub browser: crate::tools::browser::BrowserRegistry,
}

impl Runtime {
    pub fn tools(&self, cwd: &Path) -> Vec<Arc<dyn Tool>> {
        let mut tools: Vec<Arc<dyn Tool>> = vec![
            Arc::new(BashBackgroundTool { cwd: cwd.to_path_buf(), jobs: self.jobs.clone() }),
            Arc::new(BashJobTool { jobs: self.jobs.clone() }),
            Arc::new(BashCancelTool { jobs: self.jobs.clone() }),
            Arc::new(BashSessionTool { cwd: cwd.to_path_buf(), shells: self.shells.clone() }),
            Arc::new(BashSessionCloseTool { shells: self.shells.clone() }),
            Arc::new(BashSandboxTool { cwd: cwd.to_path_buf() }),
        ];
        tools.extend(self.browser.tools(cwd));
        tools
    }
}
