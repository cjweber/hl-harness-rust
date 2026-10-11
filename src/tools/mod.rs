//! Built-in tools, ported from `src/tools/index.ts` (core filesystem, shell, JSON, and Git subset).

use crate::types::{RiskLevel, ToolDeclaration};
use anyhow::{anyhow, bail, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub mod runtime;
pub mod sqlite;
pub mod web;

pub struct ToolContext {
    pub cwd: PathBuf,
}

/// Constrain a path to the working directory, mirroring the TS `within()`.
pub fn within(cwd: &Path, path: &str) -> Result<PathBuf> {
    let base = cwd.to_path_buf();
    let absolute = if Path::new(path).is_absolute() { PathBuf::from(path) } else { base.join(path) };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    bail!("Path escapes working directory: {path}");
                }
            }
            std::path::Component::CurDir => {}
            other => normalized.push(other.as_os_str()),
        }
    }
    if !normalized.starts_with(&base) {
        bail!("Path escapes working directory: {path}");
    }
    Ok(normalized)
}

pub(crate) const OUTPUT_LIMIT: usize = 20_000;

pub(crate) fn clip(value: &str, limit: usize) -> String {
    if value.len() > limit {
        let mut end = limit;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}\n[output truncated at {limit} bytes]", &value[..end])
    } else {
        value.to_string()
    }
}

const SKIP_DIRS: [&str; 3] = [".git", "node_modules", "dist"];

fn files_under(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if SKIP_DIRS.contains(&name.as_str()) {
                continue;
            }
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.is_file() {
                files.push(path);
            }
        }
    }
    files.sort();
    files
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn declaration(&self) -> &ToolDeclaration;
    fn risk(&self) -> RiskLevel;
    async fn execute(&self, args: &Value) -> Result<String>;
}

extern "C" {
    #[link_name = "kill"]
    fn kill_c(pid: i32, signal: i32) -> i32;
}

fn string_arg(args: &Value, key: &str) -> Option<String> {
    args.get(key).and_then(Value::as_str).map(str::to_string)
}

pub(crate) async fn run_process(program: &str, args: &[&str], cwd: &Path, input: Option<&str>, timeout_ms: Option<u64>) -> Result<(Option<i32>, String)> {
    let mut command = tokio::process::Command::new(program);
    command.args(args).current_dir(cwd).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped());
    if input.is_some() {
        command.stdin(std::process::Stdio::piped());
    } else {
        command.stdin(std::process::Stdio::null());
    }
    let mut child = command.spawn()?;
    if let Some(input) = input {
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(input.as_bytes()).await;
            drop(stdin);
        }
    }
    let child_id = child.id();
    let mut pinned = Box::pin(child.wait_with_output());
    let output = match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms.unwrap_or(u64::MAX)), pinned.as_mut()).await {
        Ok(result) => result?,
        Err(_) => {
            if let Some(pid) = child_id {
                unsafe {
                    kill_c(pid as i32, 15);
                }
            }
            return Ok((None, "[command timed out]".to_string()));
        }
    };
    let mut text = String::from_utf8_lossy(&output.stdout).to_string();
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    let clipped = if text.len() > OUTPUT_LIMIT { clip(&text, OUTPUT_LIMIT) } else { text };
    Ok((output.status.code(), clipped))
}

macro_rules! tool_struct {
    ($name:ident, $risk:expr, $decl:expr) => {
        struct $name {
            declaration: ToolDeclaration,
            cwd: PathBuf,
        }
        impl $name {
            fn new(cwd: &Path) -> Self {
                Self { declaration: $decl, cwd: cwd.to_path_buf() }
            }
        }

        impl $name {
            fn base(&self) -> (&ToolDeclaration, RiskLevel) {
                (&self.declaration, $risk)
            }
        }
    };
}

fn object_parameters(properties: Value, required: &[&str]) -> Value {
    json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false})
}

tool_struct!(ReadTool, RiskLevel::Safe, ToolDeclaration {
    name: "read".into(),
    description: "Read a UTF-8 text file from the project.".into(),
    parameters: object_parameters(json!({"path": {"type": "string", "description": "Project-relative file path"}}), &["path"]),
});

#[async_trait]
impl Tool for ReadTool {
    fn declaration(&self) -> &ToolDeclaration { self.base().0 }
    fn risk(&self) -> RiskLevel { self.base().1 }
    async fn execute(&self, args: &Value) -> Result<String> {
        let path = string_arg(args, "path").ok_or_else(|| anyhow!("read requires a path"))?;
        let file = within(&self.cwd, &path)?;
        let metadata = std::fs::metadata(&file)?;
        if !metadata.is_file() {
            bail!("Not a file: {path}");
        }
        if metadata.len() > 1_000_000 {
            bail!("File is too large ({} bytes): {path}", metadata.len());
        }
        Ok(clip(&std::fs::read_to_string(&file)?, OUTPUT_LIMIT))
    }
}

tool_struct!(WriteTool, RiskLevel::Write, ToolDeclaration {
    name: "write".into(),
    description: "Write a UTF-8 text file, creating parent directories when needed.".into(),
    parameters: object_parameters(json!({"path": {"type": "string"}, "content": {"type": "string"}}), &["path", "content"]),
});

#[async_trait]
impl Tool for WriteTool {
    fn declaration(&self) -> &ToolDeclaration { self.base().0 }
    fn risk(&self) -> RiskLevel { self.base().1 }
    async fn execute(&self, args: &Value) -> Result<String> {
        let path = string_arg(args, "path").ok_or_else(|| anyhow!("write requires a path"))?;
        let content = string_arg(args, "content").unwrap_or_default();
        let file = within(&self.cwd, &path)?;
        if let Some(parent) = file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&file, content.as_bytes())?;
        Ok(format!("Wrote {path} ({} bytes)", content.as_bytes().len()))
    }
}

tool_struct!(EditTool, RiskLevel::Write, ToolDeclaration {
    name: "edit".into(),
    description: "Replace one exact occurrence of oldText with newText in a UTF-8 file.".into(),
    parameters: object_parameters(json!({"path": {"type": "string"}, "oldText": {"type": "string"}, "newText": {"type": "string"}}), &["path", "oldText", "newText"]),
});

#[async_trait]
impl Tool for EditTool {
    fn declaration(&self) -> &ToolDeclaration { self.base().0 }
    fn risk(&self) -> RiskLevel { self.base().1 }
    async fn execute(&self, args: &Value) -> Result<String> {
        let path = string_arg(args, "path").ok_or_else(|| anyhow!("edit requires a path"))?;
        let old_text = string_arg(args, "oldText").filter(|text| !text.is_empty()).ok_or_else(|| anyhow!("edit requires non-empty oldText"))?;
        let new_text = string_arg(args, "newText").unwrap_or_default();
        let file = within(&self.cwd, &path)?;
        let content = std::fs::read_to_string(&file)?;
        let matches = content.split(&old_text).count() - 1;
        if matches != 1 {
            bail!("edit expected one match in {path}, found {matches}");
        }
        std::fs::write(&file, content.replacen(&old_text, &new_text, 1).as_bytes())?;
        Ok(format!("Edited {path}"))
    }
}

tool_struct!(LsTool, RiskLevel::Safe, ToolDeclaration {
    name: "ls".into(),
    description: "List entries in a project directory.".into(),
    parameters: object_parameters(json!({"path": {"type": "string"}}), &[]),
});

#[async_trait]
impl Tool for LsTool {
    fn declaration(&self) -> &ToolDeclaration { self.base().0 }
    fn risk(&self) -> RiskLevel { self.base().1 }
    async fn execute(&self, args: &Value) -> Result<String> {
        let path = string_arg(args, "path").filter(|value| !value.is_empty()).unwrap_or_else(|| ".".to_string());
        let directory = within(&self.cwd, &path)?;
        if !directory.is_dir() {
            bail!("Not a directory: {path}");
        }
        let mut entries: Vec<String> = std::fs::read_dir(&directory)?
            .flatten()
            .map(|entry| {
                let name = entry.file_name().to_string_lossy().to_string();
                format!("{}{}", if entry.path().is_dir() { "d" } else { "-" }, name)
            })
            .collect();
        entries.sort();
        Ok(clip(&entries.join("\n"), OUTPUT_LIMIT))
    }
}

tool_struct!(FindTool, RiskLevel::Safe, ToolDeclaration {
    name: "find".into(),
    description: "Find project files whose path contains a string.".into(),
    parameters: object_parameters(json!({"pattern": {"type": "string"}, "limit": {"type": "integer", "minimum": 1}}), &["pattern"]),
});

#[async_trait]
impl Tool for FindTool {
    fn declaration(&self) -> &ToolDeclaration { self.base().0 }
    fn risk(&self) -> RiskLevel { self.base().1 }
    async fn execute(&self, args: &Value) -> Result<String> {
        let pattern = string_arg(args, "pattern").unwrap_or_default();
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(100).max(1) as usize;
        let matches: Vec<String> = files_under(&self.cwd)
            .into_iter()
            .filter(|file| file.to_string_lossy().contains(&pattern))
            .take(limit)
            .map(|file| file.to_string_lossy().to_string())
            .collect();
        Ok(clip(&if matches.is_empty() { "No matches".to_string() } else { matches.join("\n") }, OUTPUT_LIMIT))
    }
}

tool_struct!(GrepTool, RiskLevel::Safe, ToolDeclaration {
    name: "grep".into(),
    description: "Search UTF-8 project files for a regular expression.".into(),
    parameters: object_parameters(json!({"pattern": {"type": "string"}, "path": {"type": "string"}, "limit": {"type": "integer", "minimum": 1}}), &["pattern"]),
});

#[async_trait]
impl Tool for GrepTool {
    fn declaration(&self) -> &ToolDeclaration { self.base().0 }
    fn risk(&self) -> RiskLevel { self.base().1 }
    async fn execute(&self, args: &Value) -> Result<String> {
        let pattern = string_arg(args, "pattern").filter(|value| !value.is_empty()).ok_or_else(|| anyhow!("grep requires a pattern"))?;
        let path = string_arg(args, "path").filter(|value| !value.is_empty()).unwrap_or_else(|| ".".to_string());
        let root = within(&self.cwd, &path)?;
        let regex = regex::Regex::new(&pattern).map_err(|error| anyhow!("Invalid grep pattern: {error}"))?;
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(100).max(1) as usize;
        let targets = if root.is_dir() { files_under(&root) } else { vec![root] };
        let mut output: Vec<String> = Vec::new();
        for file in targets {
            if std::fs::metadata(&file).map(|metadata| metadata.len() > 1_000_000).unwrap_or(true) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(&file) else { continue };
            let display = file.to_string_lossy().to_string();
            for (index, line) in content.split('\n').enumerate() {
                if regex.is_match(line) {
                    output.push(format!("{display}:{}:{line}", index + 1));
                    if output.len() >= limit {
                        return Ok(output.join("\n"));
                    }
                }
            }
        }
        Ok(clip(&if output.is_empty() { "No matches".to_string() } else { output.join("\n") }, OUTPUT_LIMIT))
    }
}

tool_struct!(RipgrepTool, RiskLevel::Safe, ToolDeclaration {
    name: "ripgrep".into(),
    description: "Search project files with ripgrep (rg), respecting .gitignore.".into(),
    parameters: object_parameters(json!({"pattern": {"type": "string"}, "path": {"type": "string"}}), &["pattern"]),
});

#[async_trait]
impl Tool for RipgrepTool {
    fn declaration(&self) -> &ToolDeclaration { self.base().0 }
    fn risk(&self) -> RiskLevel { self.base().1 }
    async fn execute(&self, args: &Value) -> Result<String> {
        let pattern = string_arg(args, "pattern").filter(|value| !value.is_empty()).ok_or_else(|| anyhow!("ripgrep requires a pattern"))?;
        let path = string_arg(args, "path").filter(|value| !value.is_empty()).unwrap_or_else(|| ".".to_string());
        let target = within(&self.cwd, &path)?;
        let (code, output) = run_process("rg", &["--line-number", "--no-heading", "--color", "never", "--regexp", &pattern, &target.to_string_lossy()], &self.cwd, None, None).await?;
        if matches!(code, Some(code) if code > 1) {
            if output.is_empty() {
                bail!("ripgrep exited with {code:?}");
            }
            bail!(output);
        }
        Ok(clip(&if output.is_empty() { "No matches".to_string() } else { output }, OUTPUT_LIMIT))
    }
}

tool_struct!(BashTool, RiskLevel::Shell, ToolDeclaration {
    name: "bash".into(),
    description: "Run a shell command in the project working directory.".into(),
    parameters: object_parameters(json!({"command": {"type": "string", "description": "Shell command"}, "timeoutMs": {"type": "integer", "minimum": 100, "maximum": 300000}}), &["command"]),
});

#[async_trait]
impl Tool for BashTool {
    fn declaration(&self) -> &ToolDeclaration { self.base().0 }
    fn risk(&self) -> RiskLevel { self.base().1 }
    async fn execute(&self, args: &Value) -> Result<String> {
        let command = string_arg(args, "command").ok_or_else(|| anyhow!("bash requires a command"))?;
        let timeout_ms = args.get("timeoutMs").and_then(Value::as_u64).unwrap_or(120_000).clamp(100, 300_000);
        let (code, output) = run_process("/bin/bash", &["-lc", &command], &self.cwd, None, Some(timeout_ms)).await?;
        let output = match code {
            Some(0) | None => output,
            Some(code) => format!("{output}\n[exit {code}]"),
        };
        Ok(clip(&output, OUTPUT_LIMIT))
    }
}

// ---------------------------------------------------------------------------
// JSON pointer tools
// ---------------------------------------------------------------------------

fn pointer_tokens(pointer: &str) -> Result<Vec<String>> {
    let Some(rest) = pointer.strip_prefix('/') else { bail!("JSON pointer must start with /") };
    Ok(rest.split('/').map(|token| token.replace("~1", "/").replace("~0", "~")).collect())
}

fn json_pointer_get(value: &Value, pointer: &str) -> Result<Value> {
    let mut current = value.clone();
    for token in pointer_tokens(pointer)? {
        let next = match &current {
            Value::Object(map) => map.get(&token).cloned(),
            Value::Array(items) => token.parse::<usize>().ok().and_then(|index| items.get(index).cloned()),
            _ => None,
        };
        match next {
            Some(next) => current = next,
            None => bail!("JSON pointer not found: {pointer}"),
        }
    }
    Ok(current)
}

fn json_parent_mut<'a>(root: &'a mut Value, tokens: &[String]) -> Result<(&'a mut Value, String)> {
    let mut current = root;
    for token in &tokens[..tokens.len() - 1] {
        current = match current {
            Value::Object(map) => map.get_mut(token).ok_or_else(|| anyhow!("JSON pointer path not found: {}", tokens.join("/")))?,
            Value::Array(items) => items.get_mut(token.parse::<usize>().map_err(|_| anyhow!("JSON pointer path not found: {}", tokens.join("/")))?).ok_or_else(|| anyhow!("JSON pointer path not found: {}", tokens.join("/")))?,
            _ => bail!("JSON pointer path not found: {}", tokens.join("/")),
        };
    }
    let key = tokens.last().cloned().unwrap_or_default();
    if matches!(key.as_str(), "__proto__" | "prototype" | "constructor") {
        bail!("Unsafe JSON pointer key");
    }
    Ok((current, key))
}

tool_struct!(JsonGetTool, RiskLevel::Safe, ToolDeclaration {
    name: "json_get".into(),
    description: "Read a JSON value using an RFC 6901 JSON pointer.".into(),
    parameters: object_parameters(json!({"path": {"type": "string"}, "pointer": {"type": "string"}}), &["path", "pointer"]),
});

#[async_trait]
impl Tool for JsonGetTool {
    fn declaration(&self) -> &ToolDeclaration { self.base().0 }
    fn risk(&self) -> RiskLevel { self.base().1 }
    async fn execute(&self, args: &Value) -> Result<String> {
        let path = string_arg(args, "path").ok_or_else(|| anyhow!("json_get requires a path"))?;
        let pointer = string_arg(args, "pointer").unwrap_or_default();
        let file = within(&self.cwd, &path)?;
        let value: Value = serde_json::from_str(&std::fs::read_to_string(&file)?)?;
        let current = json_pointer_get(&value, &pointer)?;
        Ok(clip(&serde_json::to_string_pretty(&current)?, OUTPUT_LIMIT))
    }
}

tool_struct!(JsonEditTool, RiskLevel::Write, ToolDeclaration {
    name: "json_edit".into(),
    description: "Set a JSON value using an RFC 6901 pointer and preserve pretty formatting.".into(),
    parameters: object_parameters(json!({"path": {"type": "string"}, "pointer": {"type": "string"}, "value": {}}), &["path", "pointer", "value"]),
});

#[async_trait]
impl Tool for JsonEditTool {
    fn declaration(&self) -> &ToolDeclaration { self.base().0 }
    fn risk(&self) -> RiskLevel { self.base().1 }
    async fn execute(&self, args: &Value) -> Result<String> {
        let path = string_arg(args, "path").filter(|value| !value.is_empty()).ok_or_else(|| anyhow!("json_edit requires a path"))?;
        let pointer = string_arg(args, "pointer").unwrap_or_default();
        let file = within(&self.cwd, &path)?;
        let mut value: Value = serde_json::from_str(&std::fs::read_to_string(&file)?)?;
        let tokens = pointer_tokens(&pointer)?;
        if tokens.is_empty() {
            bail!("Replacing the JSON document root is not supported");
        }
        let (parent, key) = json_parent_mut(&mut value, &tokens)?;
        match parent {
            Value::Array(items) => {
                let index = key.parse::<usize>().map_err(|_| anyhow!("JSON array index is out of range"))?;
                if index >= items.len() {
                    bail!("JSON array index is out of range");
                }
                items[index] = args["value"].clone();
            }
            Value::Object(map) => {
                map.insert(key, args["value"].clone());
            }
            _ => bail!("JSON pointer parent is not a container"),
        }
        std::fs::write(&file, format!("{}\n", serde_json::to_string_pretty(&value)?))?;
        Ok(format!("Updated JSON pointer {pointer} in {path}"))
    }
}

// ---------------------------------------------------------------------------
// Git tools
// ---------------------------------------------------------------------------

async fn scan_conflicts(cwd: &Path) -> Result<Vec<(String, Vec<(usize, usize, Vec<String>)>)>> {
    let mut paths = std::collections::BTreeSet::new();
    let (_, output) = run_process("git", &["ls-files", "--unmerged"], cwd, None, None).await?;
    for line in output.split('\n') {
        if let Some(path) = line.split('\t').nth(1).map(str::trim).filter(|value| !value.is_empty()) {
            paths.insert(path.to_string());
        }
    }
    if paths.is_empty() {
        let (_, output) = run_process("git", &["diff", "--name-only", "--diff-filter=U"], cwd, None, None).await?;
        for line in output.split('\n') {
            let path = line.trim();
            if !path.is_empty() {
                paths.insert(path.to_string());
            }
        }
    }
    let mut files = Vec::new();
    for path in paths {
        let Ok(content) = std::fs::read_to_string(within(cwd, &path)?) else { continue };
        let lines: Vec<&str> = content.split('\n').collect();
        let mut regions: Vec<(usize, usize, Vec<String>)> = Vec::new();
        let mut start: isize = -1;
        for (index, line) in lines.iter().enumerate() {
            if line.starts_with("<<<<<<<") {
                start = index as isize;
            } else if line.starts_with(">>>>>>>") && start >= 0 {
                regions.push((start as usize + 1, index + 1, lines[start as usize..=index].iter().map(|line| line.to_string()).collect()));
                start = -1;
            }
        }
        if !regions.is_empty() {
            files.push((path, regions));
        }
    }
    Ok(files)
}

fn format_conflicts(files: &[(String, Vec<(usize, usize, Vec<String>)>)]) -> String {
    let mut output = vec![format!("Conflicts in {} file(s):", files.len())];
    for (path, regions) in files {
        output.push(format!("  {path}"));
        for (start, end, lines) in regions {
            output.push(format!("    lines {start}-{end}"));
            for line in lines {
                output.push(format!("      {line}"));
            }
        }
    }
    output.join("\n")
}

tool_struct!(GitStatusTool, RiskLevel::Safe, ToolDeclaration {
    name: "git_status".into(),
    description: "Show concise Git working-tree status.".into(),
    parameters: object_parameters(json!({}), &[]),
});

#[async_trait]
impl Tool for GitStatusTool {
    fn declaration(&self) -> &ToolDeclaration { self.base().0 }
    fn risk(&self) -> RiskLevel { self.base().1 }
    async fn execute(&self, _args: &Value) -> Result<String> {
        let (code, output) = run_process("git", &["status", "--short", "--branch"], &self.cwd, None, None).await?;
        if code != Some(0) {
            if output.is_empty() {
                bail!("git status exited with {code:?}");
            }
            bail!(output);
        }
        Ok(clip(&if output.is_empty() { "(clean)".to_string() } else { output }, OUTPUT_LIMIT))
    }
}

tool_struct!(GitDiffTool, RiskLevel::Safe, ToolDeclaration {
    name: "git_diff".into(),
    description: "Show the current Git diff, optionally limited to a project-relative path.".into(),
    parameters: object_parameters(json!({"path": {"type": "string"}}), &[]),
});

#[async_trait]
impl Tool for GitDiffTool {
    fn declaration(&self) -> &ToolDeclaration { self.base().0 }
    fn risk(&self) -> RiskLevel { self.base().1 }
    async fn execute(&self, args: &Value) -> Result<String> {
        let path = string_arg(args, "path").filter(|value| !value.is_empty());
        let mut git_args: Vec<String> = vec!["diff".into(), "--no-ext-diff".into(), "--unified=3".into()];
        if let Some(path) = &path {
            let target = within(&self.cwd, path)?;
            git_args.push("--".into());
            git_args.push(target.to_string_lossy().to_string());
        }
        let git_args_refs: Vec<&str> = git_args.iter().map(|value| value.as_str()).collect();
        let (code, output) = run_process("git", &git_args_refs, &self.cwd, None, None).await?;
        if code != Some(0) {
            if output.is_empty() {
                bail!("git diff exited with {code:?}");
            }
            bail!(output);
        }
        Ok(clip(&if output.is_empty() { "(no diff)".to_string() } else { output }, OUTPUT_LIMIT))
    }
}

tool_struct!(GitLogTool, RiskLevel::Safe, ToolDeclaration {
    name: "git_log".into(),
    description: "Show recent commits with hashes and summaries.".into(),
    parameters: object_parameters(json!({"limit": {"type": "integer", "minimum": 1, "maximum": 100}}), &[]),
});

#[async_trait]
impl Tool for GitLogTool {
    fn declaration(&self) -> &ToolDeclaration { self.base().0 }
    fn risk(&self) -> RiskLevel { self.base().1 }
    async fn execute(&self, args: &Value) -> Result<String> {
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20).clamp(1, 100);
        let (code, output) = run_process("git", &["log", "-n", &limit.to_string(), "--oneline", "--decorate"], &self.cwd, None, None).await?;
        if code != Some(0) {
            if output.is_empty() {
                bail!("git log exited with {code:?}");
            }
            bail!(output);
        }
        Ok(clip(&if output.is_empty() { "(no commits)".to_string() } else { output }, OUTPUT_LIMIT))
    }
}

tool_struct!(GitApplyCheckTool, RiskLevel::Safe, ToolDeclaration {
    name: "git_apply_check".into(),
    description: "Check whether a unified Git patch applies without changing files.".into(),
    parameters: object_parameters(json!({"patch": {"type": "string"}}), &["patch"]),
});

#[async_trait]
impl Tool for GitApplyCheckTool {
    fn declaration(&self) -> &ToolDeclaration { self.base().0 }
    fn risk(&self) -> RiskLevel { self.base().1 }
    async fn execute(&self, args: &Value) -> Result<String> {
        let patch = string_arg(args, "patch").filter(|value| !value.is_empty()).ok_or_else(|| anyhow!("git_apply_check requires a patch"))?;
        let (code, output) = run_process("git", &["apply", "--check", "-"], &self.cwd, Some(&patch), None).await?;
        if code != Some(0) {
            if output.is_empty() {
                bail!("Git patch does not apply cleanly");
            }
            bail!(output);
        }
        Ok("Git patch applies cleanly".to_string())
    }
}

tool_struct!(GitApplyTool, RiskLevel::Write, ToolDeclaration {
    name: "git_apply".into(),
    description: "Apply a unified Git patch; requires write approval and may fail on conflicts.".into(),
    parameters: object_parameters(json!({"patch": {"type": "string"}}), &["patch"]),
});

#[async_trait]
impl Tool for GitApplyTool {
    fn declaration(&self) -> &ToolDeclaration { self.base().0 }
    fn risk(&self) -> RiskLevel { self.base().1 }
    async fn execute(&self, args: &Value) -> Result<String> {
        let patch = string_arg(args, "patch").filter(|value| !value.is_empty()).ok_or_else(|| anyhow!("git_apply requires a patch"))?;
        let (code, output) = run_process("git", &["apply", "--3way", "-"], &self.cwd, Some(&patch), None).await?;
        if code != Some(0) {
            let conflicts = scan_conflicts(&self.cwd).await?;
            let mut message = vec![if output.is_empty() { "Git patch failed".to_string() } else { output }];
            if !conflicts.is_empty() {
                message.push(format_conflicts(&conflicts));
            } else {
                message.push("No conflict markers found; the patch may not apply to this tree.".to_string());
            }
            bail!("{}", message.join("\n"));
        }
        Ok("Git patch applied".to_string())
    }
}

tool_struct!(GitConflictsTool, RiskLevel::Safe, ToolDeclaration {
    name: "git_conflicts".into(),
    description: "List unmerged files and their conflict marker regions with line numbers.".into(),
    parameters: object_parameters(json!({}), &[]),
});

#[async_trait]
impl Tool for GitConflictsTool {
    fn declaration(&self) -> &ToolDeclaration { self.base().0 }
    fn risk(&self) -> RiskLevel { self.base().1 }
    async fn execute(&self, _args: &Value) -> Result<String> {
        let files = scan_conflicts(&self.cwd).await?;
        if files.is_empty() {
            return Ok("No merge conflicts".to_string());
        }
        Ok(clip(&format_conflicts(&files), OUTPUT_LIMIT))
    }
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clip_truncates_on_char_boundary() {
        let output = clip(&"x".repeat(30), 20);
        assert!(output.starts_with(&"x".repeat(19)));
        assert!(output.contains("output truncated"));
    }
}


// ---------------------------------------------------------------------------
// Web tool wrappers
// ---------------------------------------------------------------------------

pub struct WebSearchTool;

#[async_trait]
impl Tool for WebSearchTool {
    fn declaration(&self) -> &ToolDeclaration {
        static DECL: std::sync::OnceLock<ToolDeclaration> = std::sync::OnceLock::new();
        DECL.get_or_init(|| ToolDeclaration {
            name: "web_search".into(),
            description: "Search the web and return titles, URLs, and snippets. Uses BRAVE_SEARCH_API_KEY when set, otherwise DuckDuckGo HTML.".into(),
            parameters: json!({"type": "object", "properties": {"query": {"type": "string"}, "maxResults": {"type": "integer", "minimum": 1, "maximum": 20}}, "required": ["query"], "additionalProperties": false}),
        })
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Network
    }

    async fn execute(&self, args: &Value) -> Result<String> {
        let query = args.get("query").and_then(Value::as_str).unwrap_or_default().to_string();
        let max_results = args.get("maxResults").and_then(Value::as_u64).unwrap_or(5) as usize;
        web::web_search(&query, max_results).await
    }
}

pub struct WebFetchTool;

#[async_trait]
impl Tool for WebFetchTool {
    fn declaration(&self) -> &ToolDeclaration {
        static DECL: std::sync::OnceLock<ToolDeclaration> = std::sync::OnceLock::new();
        DECL.get_or_init(|| ToolDeclaration {
            name: "web_fetch".into(),
            description: "Fetch an HTTP(S) URL and return its text or JSON response.".into(),
            parameters: json!({"type": "object", "properties": {"url": {"type": "string"}, "maxBytes": {"type": "integer", "minimum": 1}}, "required": ["url"], "additionalProperties": false}),
        })
    }

    fn risk(&self) -> RiskLevel {
        RiskLevel::Network
    }

    async fn execute(&self, args: &Value) -> Result<String> {
        let url = args.get("url").and_then(Value::as_str).unwrap_or_default().to_string();
        let max_bytes = args.get("maxBytes").and_then(Value::as_u64).unwrap_or(100_000) as usize;
        web::web_fetch(&url, max_bytes).await
    }
}

/// Alias kept for tests; identical to `create_tools`.
pub fn builtin_tools(cwd: &Path) -> Vec<std::sync::Arc<dyn Tool>> {
    create_tools(cwd)
}

/// Full tool set with shared runtime state, mirroring the TS `createTools()`.
pub fn create_tools(cwd: &Path) -> Vec<std::sync::Arc<dyn Tool>> {
    static RUNTIME: std::sync::OnceLock<runtime::Runtime> = std::sync::OnceLock::new();
    let runtime = RUNTIME.get_or_init(runtime::Runtime::default);
    let mut tools: Vec<std::sync::Arc<dyn Tool>> = vec![
        std::sync::Arc::new(ReadTool::new(cwd)),
        std::sync::Arc::new(WriteTool::new(cwd)),
        std::sync::Arc::new(EditTool::new(cwd)),
        std::sync::Arc::new(LsTool::new(cwd)),
        std::sync::Arc::new(FindTool::new(cwd)),
        std::sync::Arc::new(GrepTool::new(cwd)),
        std::sync::Arc::new(RipgrepTool::new(cwd)),
        std::sync::Arc::new(BashTool::new(cwd)),
        std::sync::Arc::new(JsonGetTool::new(cwd)),
        std::sync::Arc::new(JsonEditTool::new(cwd)),
        std::sync::Arc::new(GitStatusTool::new(cwd)),
        std::sync::Arc::new(GitDiffTool::new(cwd)),
        std::sync::Arc::new(GitLogTool::new(cwd)),
        std::sync::Arc::new(GitApplyCheckTool::new(cwd)),
        std::sync::Arc::new(GitApplyTool::new(cwd)),
        std::sync::Arc::new(GitConflictsTool::new(cwd)),
        std::sync::Arc::new(WebSearchTool),
        std::sync::Arc::new(WebFetchTool),
        std::sync::Arc::new(sqlite::DatabaseQueryTool { cwd: cwd.to_path_buf() }),
        std::sync::Arc::new(sqlite::DatabaseExecuteTool { cwd: cwd.to_path_buf() }),
    ];
    tools.extend(runtime.tools(cwd));
    tools
}
