//! Pi-compatible JSONL v3 session store, ported from `src/sessions/store.ts`.

use crate::types::{ImageAttachment, Message, Role, ToolCall};
use anyhow::{anyhow, bail, Result};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug)]
pub struct SessionHeader {
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    pub parent_session: Option<String>,
}

fn now() -> String {
    chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true)
}

fn safe_project(cwd: &str) -> String {
    let trimmed = cwd.trim_start_matches(['/', '\\']);
    let replaced: String = trimmed.chars().map(|c| if c == '/' || c == '\\' || c == ':' { '-' } else { c }).collect();
    format!("--{replaced}--")
}

pub fn default_session_path(cwd: &Path, id: &str) -> PathBuf {
    let stamp = now().replace([':', '.'], "-");
    let home = std::env::var("HOME").map(PathBuf::from).unwrap_or_else(|_| PathBuf::from("/"));
    home.join(".pi").join("agent").join("sessions").join(safe_project(&cwd.to_string_lossy())).join(format!("{stamp}_{id}.jsonl"))
}

fn entry_id() -> String {
    uuid::Uuid::new_v4().to_string()[..8].to_string()
}

fn text_from_content(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(blocks) => blocks
            .iter()
            .filter(|block| block["type"] == "text")
            .filter_map(|block| block["text"].as_str())
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

fn text_blocks(message: &Message) -> Value {
    let mut blocks = Vec::new();
    if !message.content.is_empty() {
        blocks.push(json!({"type": "text", "text": message.content}));
    }
    for call in &message.tool_calls {
        blocks.push(json!({"type": "toolCall", "id": call.id, "name": call.name, "arguments": call.arguments}));
    }
    json!(blocks)
}

fn to_stored_message(message: &Message) -> Value {
    let timestamp = chrono::Utc::now().timestamp_millis();
    match message.role {
        Role::User => {
            let mut stored = json!({"role": "user", "content": message.content, "timestamp": timestamp});
            if !message.images.is_empty() {
                stored["images"] = json!(message.images.iter().map(|image| json!({"mimeType": image.mime_type, "data": image.data})).collect::<Vec<_>>());
            }
            stored
        }
        Role::Assistant => json!({"role": "assistant", "content": text_blocks(message), "timestamp": timestamp}),
        Role::System => json!({"role": "system", "content": message.content, "timestamp": timestamp}),
        Role::Tool => json!({
            "role": "toolResult",
            "toolCallId": message.tool_call_id.clone().unwrap_or_default(),
            "toolName": message.tool_name.clone().unwrap_or_else(|| "tool".to_string()),
            "content": [{"type": "text", "text": message.content}],
            "isError": false,
            "timestamp": timestamp,
        }),
    }
}

fn from_stored_message(message: &Value) -> Option<Message> {
    let role = message["role"].as_str()?;
    match role {
        "system" => None,
        "user" => {
            let mut user = Message::user(text_from_content(&message["content"]));
            user.images = message["images"]
                .as_array()
                .map(|images| {
                    images
                        .iter()
                        .map(|image| ImageAttachment { mime_type: image["mimeType"].as_str().unwrap_or("image/png").to_string(), data: image["data"].as_str().unwrap_or_default().to_string() })
                        .collect()
                })
                .unwrap_or_default();
            Some(user)
        }
        "toolResult" => Some(Message::tool(
            text_from_content(&message["content"]),
            message["toolCallId"].as_str().unwrap_or_default(),
            message["toolName"].as_str().unwrap_or("tool"),
        )),
        "assistant" => {
            let tool_calls: Vec<ToolCall> = message["content"]
                .as_array()
                .map(|blocks| {
                    blocks
                        .iter()
                        .filter(|block| block["type"] == "toolCall")
                        .filter_map(|block| {
                            let arguments = block["arguments"].as_object().cloned()?;
                            Some(ToolCall { id: block["id"].as_str()?.to_string(), name: block["name"].as_str()?.to_string(), arguments })
                        })
                        .collect()
                })
                .unwrap_or_default();
            let mut assistant = Message::assistant(text_from_content(&message["content"]));
            assistant.tool_calls = tool_calls;
            Some(assistant)
        }
        _ => None,
    }
}

pub struct SessionStore {
    pub header: SessionHeader,
    pub path: PathBuf,
    entries: Vec<Value>,
    current_leaf_id: Option<String>,
    file: std::fs::File,
}

impl SessionStore {
    fn from_parts(path: PathBuf, header: SessionHeader, entries: Vec<Value>, file: std::fs::File) -> Self {
        let current_leaf_id = entries.last().and_then(|entry| entry["id"].as_str().map(str::to_string));
        Self { header, path, entries, current_leaf_id, file }
    }

    pub fn create(cwd: &Path) -> Result<Self> {
        let header = SessionHeader {
            id: uuid::Uuid::new_v4().to_string(),
            timestamp: now(),
            cwd: cwd.to_string_lossy().to_string(),
            parent_session: None,
        };
        let path = default_session_path(cwd, &header.id);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(&path)?;
        use std::io::Write;
        writeln!(file, "{}", json!({"type": "session", "version": 3, "id": header.id, "timestamp": header.timestamp, "cwd": header.cwd}))?;
        Ok(Self::from_parts(path, header, Vec::new(), file))
    }

    pub fn open(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path).map_err(|_| anyhow!("Session does not exist: {}", path.display()))?;
        let lines: Vec<&str> = content.lines().filter(|line| !line.is_empty()).collect();
        let header_value: Value = serde_json::from_str(lines.first().copied().unwrap_or("{}"))?;
        if header_value["type"] != "session" || header_value["version"].as_u64() != Some(3) {
            bail!("Only Pi session format v3 is supported");
        }
        let header = SessionHeader {
            id: header_value["id"].as_str().unwrap_or_default().to_string(),
            timestamp: header_value["timestamp"].as_str().unwrap_or_default().to_string(),
            cwd: header_value["cwd"].as_str().unwrap_or_default().to_string(),
            parent_session: header_value["parentSession"].as_str().map(str::to_string),
        };
        let entries: Vec<Value> = lines[1..].iter().map(|line| serde_json::from_str::<Value>(line).map_err(|error| anyhow!("Invalid session entry: {error}"))).collect::<Result<_>>()?;
        let file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self::from_parts(path.to_path_buf(), header, entries, file))
    }

    pub fn all_entries(&self) -> &[Value] {
        &self.entries
    }

    pub fn leaf_id(&self) -> Option<&str> {
        self.current_leaf_id.as_deref()
    }

    pub fn get_entry(&self, id: &str) -> Option<&Value> {
        self.entries.iter().find(|entry| entry["id"].as_str() == Some(id))
    }

    pub fn get_children(&self, parent_id: Option<&str>) -> Vec<Value> {
        self.entries
            .iter()
            .filter(|entry| entry["parentId"].as_str() == parent_id || (parent_id.is_none() && entry["parentId"].is_null()))
            .cloned()
            .collect()
    }

    pub fn all_cache_entries(&self) -> Vec<Value> {
        self.entries.iter().filter(|entry| entry["type"] == "cache").cloned().collect()
    }

    pub fn set_leaf(&mut self, id: Option<&str>) -> Result<()> {
        if let Some(id) = id {
            if self.get_entry(id).is_none() {
                bail!("Unknown session entry: {id}");
            }
        }
        self.current_leaf_id = id.map(str::to_string);
        Ok(())
    }

    /// Branch from `from_id` (default: current leaf) back to the root, oldest first.
    pub fn get_branch(&self, from_id: Option<&str>) -> Result<Vec<Value>> {
        let mut branch = Vec::new();
        let mut current = from_id.map(str::to_string).or_else(|| self.current_leaf_id.clone());
        let mut seen = std::collections::HashSet::new();
        while let Some(ref id) = current {
            if !seen.insert(id.clone()) {
                bail!("Cycle in session tree");
            }
            let entry = self.get_entry(id).ok_or_else(|| anyhow!("Unknown session entry: {id}"))?.clone();
            current = entry["parentId"].as_str().map(str::to_string);
            branch.push(entry);
        }
        branch.reverse();
        Ok(branch)
    }

    fn is_compaction(entry: &Value) -> bool {
        entry["type"] == "compaction" && entry["summary"].is_string() && (entry["firstKeptEntryId"].is_null() || entry["firstKeptEntryId"].is_string())
    }

    /// Active context: apply the most recent compaction and keep the post-compaction span.
    pub fn context_entries(&self) -> Result<Vec<Value>> {
        let branch = self.get_branch(None)?;
        let compaction_index = branch.iter().rposition(Self::is_compaction);
        let Some(compaction_index) = compaction_index else { return Ok(branch) };
        let compaction = &branch[compaction_index];
        let first_kept = compaction["firstKeptEntryId"].as_str();
        let first_kept_index = first_kept.and_then(|id| branch.iter().position(|entry| entry["id"] == *id));
        let kept = match first_kept_index {
            Some(index) if index < compaction_index => branch[index..compaction_index].to_vec(),
            _ => Vec::new(),
        };
        Ok([vec![compaction.clone()], kept, branch[compaction_index + 1..].to_vec()].concat())
    }

    pub fn messages(&self) -> Result<Vec<Message>> {
        let active = self.context_entries()?;
        let mut edits: Map<String, Value> = Map::new();
        for entry in &active {
            if entry["type"] == "context_edit" {
                if let Some(target) = entry["targetId"].as_str() {
                    edits.insert(target.to_string(), entry["replacement"].clone());
                }
            }
        }
        let mut messages = Vec::new();
        for entry in &active {
            if Self::is_compaction(entry) {
                messages.push(Message::system(entry["summary"].as_str().unwrap_or_default()));
                continue;
            }
            if entry["type"] == "custom_message" {
                messages.push(Message::user(entry["content"].as_str().unwrap_or_default()));
                continue;
            }
            if entry["type"] != "message" {
                continue;
            }
            let Some(message) = from_stored_message(&entry["message"]) else { continue };
            let Some(id) = entry["id"].as_str() else { continue };
            match edits.get(id) {
                None => messages.push(message),
                Some(replacement) => {
                    if !replacement.is_null() {
                        messages.push(Message { content: replacement["content"].as_str().unwrap_or_default().to_string(), ..message });
                    }
                }
            }
        }
        Ok(messages)
    }

    fn append_entry(&mut self, mut entry: Value) -> Result<String> {
        entry["id"] = json!(entry_id());
        entry["parentId"] = json!(self.current_leaf_id.clone());
        entry["timestamp"] = json!(now());
        use std::io::Write;
        writeln!(self.file, "{entry}")?;
        let id = entry["id"].as_str().unwrap_or_default().to_string();
        self.current_leaf_id = Some(id.clone());
        self.entries.push(entry);
        Ok(id)
    }

    pub fn append_message(&mut self, message: &Message) -> Result<String> {
        let entry = json!({"type": "message", "message": to_stored_message(message)});
        self.append_entry(entry)
    }

    pub fn append_usage(&mut self, kind: &str, prompt_tokens: u64, completion_tokens: u64, cached_tokens: u64) -> Result<String> {
        let entry = json!({"type": "usage", "kind": kind, "usage": {"promptTokens": prompt_tokens, "completionTokens": completion_tokens, "cachedTokens": cached_tokens}});
        self.append_entry(entry)
    }

    pub fn append_cache(&mut self, provider: &str, model: &str, prefix_hash: &str, prefix_tokens: usize, suffix_tokens: usize, prompt_tokens: u64, completion_tokens: u64, cached_tokens: u64, latency_ms: Option<u64>) -> Result<String> {
        let mut entry = json!({
            "type": "cache",
            "provider": provider,
            "model": model,
            "prefixHash": prefix_hash,
            "prefixTokens": prefix_tokens,
            "suffixTokens": suffix_tokens,
            "usage": {"promptTokens": prompt_tokens, "completionTokens": completion_tokens, "cachedTokens": cached_tokens},
        });
        if let Some(latency_ms) = latency_ms {
            entry["latencyMs"] = json!(latency_ms);
        }
        self.append_entry(entry)
    }

    pub fn append_context_edit(&mut self, target_id: &str, replacement: Option<&str>) -> Result<String> {
        if self.get_entry(target_id).is_none() {
            bail!("Unknown context edit target: {target_id}");
        }
        let replacement = replacement.map(|content| json!({"content": content})).unwrap_or(Value::Null);
        self.append_entry(json!({"type": "context_edit", "targetId": target_id, "replacement": replacement}))
    }

    pub fn append_compaction(&mut self, summary: &str, first_kept_entry_id: Option<&str>, tokens_before: usize) -> Result<String> {
        self.append_entry(json!({
            "type": "compaction",
            "summary": summary,
            "firstKeptEntryId": first_kept_entry_id,
            "tokensBefore": tokens_before,
        }))
    }

    pub fn append_branch_summary(&mut self, from_id: &str, summary: &str) -> Result<String> {
        if self.get_entry(from_id).is_none() {
            bail!("Unknown branch summary source: {from_id}");
        }
        self.append_entry(json!({"type": "branch_summary", "fromId": from_id, "summary": summary}))
    }

    pub fn append_custom(&mut self, custom_type: &str, data: Value) -> Result<String> {
        self.append_entry(json!({"type": "custom", "customType": custom_type, "data": data}))
    }

    pub fn append_custom_message(&mut self, custom_type: &str, content: &str, display: bool) -> Result<String> {
        self.append_entry(json!({"type": "custom_message", "customType": custom_type, "content": content, "display": display}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("harness-sessions-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[tokio::test]
    async fn round_trips_messages_and_projects_context() -> Result<()> {
        let dir = temp_dir();
        std::env::set_var("HOME", &dir);
        let mut store = SessionStore::create(&dir)?;
        let first = store.append_message(&Message::user("hello"))?;
        store.append_message(&Message::assistant("hi there"))?;
        let branch_point = store.leaf_id().unwrap().to_string();
        store.append_message(&Message::user("bad path"))?;
        // Branch: make the leaf earlier and diverge.
        store.set_leaf(Some(&branch_point))?;
        store.append_message(&Message::user("good path"))?;
        store.append_context_edit(&first, Some("edited hello"))?;
        let messages = store.messages()?;
        let contents: Vec<&str> = messages.iter().map(|message| message.content.as_str()).collect();
        assert_eq!(contents, vec!["edited hello", "hi there", "good path"]);
        std::env::remove_var("HOME");
        Ok(())
    }

    #[tokio::test]
    async fn compaction_replaces_history() -> Result<()> {
        let dir = temp_dir();
        std::env::set_var("HOME", &dir);
        let mut store = SessionStore::create(&dir)?;
        store.append_message(&Message::user("old one"))?;
        store.append_message(&Message::user("old two"))?;
        let first = store.get_children(None)[0]["id"].as_str().unwrap().to_string();
        store.append_compaction("summary of old", Some(&first), 100)?;
        store.append_message(&Message::user("new"))?;
        let messages = store.messages()?;
        let contents: Vec<&str> = messages.iter().map(|message| message.content.as_str()).collect();
        assert_eq!(contents, vec!["summary of old", "old one", "old two", "new"]);
        std::env::remove_var("HOME");
        Ok(())
    }
}
