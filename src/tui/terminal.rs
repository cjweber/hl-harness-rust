//! Interactive terminal shell, ported from the `TerminalUi` class in
//! `src/tui/screen.ts`: shared state, drawing, byte-level input handling,
//! the approval modal, pending inputs (MCP prompts), completions, history,
//! scrollback, and workspace widget controls.

use super::diff_view::{parse_diff, DiffView};
use super::render::{self, CacheHistory, CacheHistoryEntry, CacheUsageView, DashboardState, WorkspaceRenderState, ERASE_ROW};
use super::workspace::{normalize_workspace, widget_at, WorkspaceLayout};
use crate::types::CachePlan;
use anyhow::{anyhow, Result as AnyResult};
use crossterm::terminal;
use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Debug, PartialEq)]
pub enum Intent {
    Submit(String),
    Cancel,
    Quit,
}

pub type Intents = tokio::sync::mpsc::UnboundedSender<Intent>;
pub type PendingInput = Option<(String, tokio::sync::oneshot::Sender<Option<String>>)>;
pub type PendingApproval = Option<(String, String, tokio::sync::oneshot::Sender<bool>)>;

#[derive(Clone, Copy, PartialEq)]
enum WidgetMode {
    Move,
    Resize,
}

pub struct TerminalUi {
    input: Mutex<String>,
    transcript: Mutex<Vec<String>>,
    status: Mutex<String>,
    cache: Mutex<Option<CachePlan>>,
    cache_usage: Mutex<Option<CacheUsageView>>,
    secret_mode: AtomicBool,
    working: AtomicBool,
    work_frame: AtomicUsize,
    scroll_offset: Mutex<usize>,
    history: Mutex<Vec<String>>,
    history_index: Mutex<isize>,
    completions: Mutex<Vec<String>>,
    workspace: Mutex<Option<WorkspaceLayout>>,
    workspace_values: Mutex<Vec<(String, String)>>,
    diff_view: Mutex<Option<DiffView>>,
    cache_history: Mutex<Option<CacheHistory>>,
    conflicts: Mutex<Option<Vec<String>>>,
    selected_widget: Mutex<usize>,
    widget_mode: Mutex<Option<WidgetMode>>,
    dragging: Mutex<Option<(String, usize, usize, usize, usize)>>,
    on_workspace_change: Mutex<Option<Box<dyn Fn(&WorkspaceLayout) + Send + Sync>>>,
    pending_input: Mutex<PendingInput>,
    approval: Mutex<PendingApproval>,
    intents: Intents,
    workspace_config: Mutex<(std::path::PathBuf, std::path::PathBuf, bool)>,
    quit: Arc<AtomicBool>,
    cache_history_watch: Mutex<Option<tokio::sync::oneshot::Sender<String>>>,
}

const ESC: &str = "\u{1b}";
const CURSOR_HIDE: &str = "\u{1b}[?25l";
const CURSOR_SHOW: &str = "\u{1b}[?25h";
const MOUSE_ENABLE: &str = "\u{1b}[?1000h\u{1b}[?1006h";
const MOUSE_DISABLE: &str = "\u{1b}[?1006l\u{1b}[?1000l";

impl TerminalUi {
    pub fn new(intents: Intents) -> Arc<Self> {
        Arc::new(Self {
            input: Mutex::new(String::new()),
            transcript: Mutex::new(Vec::new()),
            status: Mutex::new("Ready".to_string()),
            cache: Mutex::new(None),
            cache_usage: Mutex::new(None),
            secret_mode: AtomicBool::new(false),
            working: AtomicBool::new(false),
            work_frame: AtomicUsize::new(0),
            scroll_offset: Mutex::new(0),
            history: Mutex::new(Vec::new()),
            history_index: Mutex::new(-1),
            completions: Mutex::new(Vec::new()),
            workspace: Mutex::new(None),
            workspace_values: Mutex::new(Vec::new()),
            diff_view: Mutex::new(None),
            cache_history: Mutex::new(None),
            conflicts: Mutex::new(None),
            selected_widget: Mutex::new(0),
            widget_mode: Mutex::new(None),
            dragging: Mutex::new(None),
            on_workspace_change: Mutex::new(None),
            pending_input: Mutex::new(None),
            approval: Mutex::new(None),
            intents,
            workspace_config: Mutex::new((std::path::PathBuf::new(), std::path::PathBuf::new(), false)),
            quit: Arc::new(AtomicBool::new(false)),
            cache_history_watch: Mutex::new(None),
        })
    }

    pub fn quit_flag(&self) -> Arc<AtomicBool> {
        self.quit.clone()
    }

    pub fn transcript(&self) -> Vec<String> {
        self.transcript.lock().unwrap().clone()
    }

    pub fn set_transcript(&self, lines: Vec<String>) {
        *self.transcript.lock().unwrap() = lines;
        *self.scroll_offset.lock().unwrap() = 0;
        self.draw();
    }

    pub fn push_transcript(&self, line: String) {
        {
            let mut transcript = self.transcript.lock().unwrap();
            transcript.push(line);
        }
        if *self.scroll_offset.lock().unwrap() == 0 {
            self.draw();
        }
    }

    pub fn append_transcript(&self, lines: Vec<String>) {
        {
            let mut transcript = self.transcript.lock().unwrap();
            transcript.extend(lines);
        }
        if *self.scroll_offset.lock().unwrap() == 0 {
            self.draw();
        }
    }

    pub fn set_status(&self, status: &str) {
        *self.status.lock().unwrap() = status.to_string();
        self.draw();
    }

    pub fn set_cache(&self, plan: CachePlan) {
        *self.cache.lock().unwrap() = Some(plan);
        self.draw();
    }

    pub fn set_cache_usage(&self, prompt_tokens: u64, cached_tokens: u64, prefix_hash: Option<&str>, latency_ms: Option<u64>) {
        {
            let mut usage = self.cache_usage.lock().unwrap();
            let requests = usage.as_ref().map(|entry| entry.requests).unwrap_or(0) + 1;
            *usage = Some(CacheUsageView { prompt_tokens, cached_tokens, requests });
        }
        {
            let mut history = self.cache_history.lock().unwrap();
            if let Some(history) = history.as_mut() {
                let fallback = self.cache.lock().unwrap().as_ref().map(|plan| plan.prefix_hash.clone()).unwrap_or_else(|| "unknown".into());
                history.entries.push(CacheHistoryEntry {
                    prefix_hash: prefix_hash.map(str::to_string).unwrap_or(fallback),
                    prompt_tokens,
                    cached_tokens,
                    latency_ms,
                });
                if history.entries.len() > 500 {
                    history.entries.remove(0);
                }
                history.index = history.entries.len() - 1;
            }
        }
        self.draw();
    }

    pub fn set_cache_history(&self, entries: Vec<CacheHistoryEntry>) {
        let index = entries.len();
        *self.cache_history.lock().unwrap() = Some(CacheHistory { entries, index: index.saturating_sub(1) });
        self.draw();
    }

    pub fn cache_command(&self, command: &str) -> String {
        let mut updated = false;
        let outcome = {
            if command.is_empty() {
                "Cache monitor opened"
            } else if command == "close" {
                *self.cache_history.lock().unwrap() = None;
                updated = true;
                "Cache monitor closed"
            } else if command != "next" && command != "prev" {
                "Unknown cache command"
            } else {
                let mut history = self.cache_history.lock().unwrap();
                match history.as_mut() {
                    Some(history) => {
                        let count = history.entries.len();
                        if count == 0 {
                            "Cache history is empty"
                        } else {
                            let direction: isize = if command == "next" { 1 } else { -1 };
                            history.index = ((history.index as isize + direction + count as isize) % count as isize) as usize;
                            drop(history);
                            updated = true;
                            "Cache monitor updated"
                        }
                    }
                    None => "No cache history loaded",
                }
            }
        };
        if updated {
            self.draw();
        }
        outcome.to_string()
    }

    pub fn has_conflicts(&self) -> bool {
        self.conflicts.lock().unwrap().as_ref().map(|report| !report.is_empty()).unwrap_or(false)
    }

    pub fn conflict_report(&self) -> String {
        self.conflicts.lock().unwrap().clone().unwrap_or_default().join("\n")
    }

    pub fn set_conflicts(&self, report: Option<String>) {
        *self.conflicts.lock().unwrap() = report.filter(|report| !report.is_empty()).map(|report| report.split('\n').map(str::to_string).collect());
        self.draw();
    }

    pub fn set_diff(&self, value: String) {
        *self.diff_view.lock().unwrap() = (!value.is_empty()).then(|| parse_diff(&value));
        self.draw();
    }

    pub fn diff_command(&self, command: &str) -> String {
        let Some(view) = self.diff_view.lock().unwrap().clone() else { return "No diff loaded".to_string() };
        let mut view = view;
        if view.files.is_empty() {
            return "No diff loaded".to_string();
        }
        let outcome = if command == "close" {
            *self.diff_view.lock().unwrap() = None;
            self.draw();
            return "Diff navigation updated".to_string();
        } else if command == "split" || command == "unified" {
            view.mode = command.to_string();
            "Diff navigation updated"
        } else if command == "next" || command == "prev" || command.starts_with("file ") || command.starts_with("hunk ") {
            let direction: isize = if command.ends_with("next") { 1 } else { -1 };
            if command.starts_with("file ") {
                view.file_index = ((view.file_index as isize + direction + view.files.len() as isize) % view.files.len() as isize) as usize;
                view.hunk_index = 0;
            } else if command.starts_with("hunk ") {
                let count = view.files[view.file_index.min(view.files.len() - 1)].hunks.len();
                if count > 0 {
                    view.hunk_index = ((view.hunk_index as isize + direction + count as isize) % count as isize) as usize;
                }
            } else {
                let hunks = view.files[view.file_index.min(view.files.len() - 1)].hunks.len();
                let moved = view.hunk_index as isize + direction;
                if moved >= 0 && (moved as usize) < hunks {
                    view.hunk_index = moved as usize;
                } else {
                    let next_index = (view.file_index as isize + direction + view.files.len() as isize) % view.files.len() as isize;
                    view.file_index = next_index as usize;
                    if hunks == 0 {
                        view.hunk_index = 0;
                    } else {
                        view.hunk_index = view.hunk_index.min(view.files[view.file_index].hunks.len() - 1);
                    }
                }
            }
            "Diff navigation updated"
        } else {
            "Unknown diff command"
        };
        *self.diff_view.lock().unwrap() = Some(view);
        self.draw();
        outcome.to_string()
    }

    pub fn set_completions(&self, values: Vec<String>) {
        *self.completions.lock().unwrap() = values;
        self.draw();
    }

    pub fn set_secret_mode(&self, enabled: bool) {
        self.secret_mode.store(enabled, Ordering::SeqCst);
        self.input.lock().unwrap().clear();
        self.draw();
    }

    pub fn set_working(&self, enabled: bool, ui: Arc<TerminalUi>) {
        self.working.store(enabled, Ordering::SeqCst);
        if enabled {
            self.work_frame.store(0, Ordering::SeqCst);
            std::thread::spawn(move || {
                loop {
                    std::thread::sleep(Duration::from_millis(180));
                    if !ui.working.load(Ordering::SeqCst) {
                        break;
                    }
                    let frame = (ui.work_frame.fetch_add(1, Ordering::SeqCst) + 1) % 4;
                    ui.work_frame.store(frame, Ordering::SeqCst);
                    if ui.working.load(Ordering::SeqCst) {
                        ui.draw();
                    }
                }
            });
        }
        self.draw();
    }

    pub fn set_workspace(&self, layout: &WorkspaceLayout, values: Vec<(String, String)>) {
        *self.workspace.lock().unwrap() = Some(layout.clone());
        *self.workspace_values.lock().unwrap() = values;
        self.draw();
    }

    pub fn set_on_workspace_change(&self, callback: impl Fn(&WorkspaceLayout) + Send + Sync + 'static) {
        *self.on_workspace_change.lock().unwrap() = Some(Box::new(callback));
    }

    pub fn set_workspace_config(&self, pi_config_dir: std::path::PathBuf, cwd: std::path::PathBuf, project_trusted: bool) {
        *self.workspace_config.lock().unwrap() = (pi_config_dir, cwd, project_trusted);
    }

    fn update_widget(&self, id: &str, x: isize, y: isize, width: isize, height: isize) {
        let Some(mut layout) = self.workspace.lock().unwrap().clone() else { return };
        let Some(widget) = layout.widgets.iter_mut().find(|candidate| candidate.id == id) else { return };
        let width = width.clamp(10, 100) as usize;
        let height = height.clamp(2, 20) as usize;
        widget.x = x.clamp(0, 100 - width as isize).max(0) as usize;
        widget.y = y.clamp(0, 100 - height as isize).max(0) as usize;
        widget.width = width;
        widget.height = height;
        if let Some(on_change) = self.on_workspace_change.lock().unwrap().as_ref() {
            on_change(&layout);
        }
        *self.workspace.lock().unwrap() = Some(layout);
        self.draw();
    }

    pub async fn request_approval(&self, summary: &str, risk: &str) -> (bool, bool) {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        *self.approval.lock().unwrap() = Some((summary.to_string(), risk.to_string(), sender));
        self.draw();
        // Approval decisions arrive through on_input; wait for the response.
        match tokio::time::timeout(Duration::from_secs(60 * 30), receiver).await {
            Ok(Ok(allow)) => (allow, false),
            _ => (false, false),
        }
    }

    pub async fn request_input(&self, prompt: &str) -> anyhow::Result<Option<String>> {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        *self.pending_input.lock().unwrap() = Some((prompt.to_string(), sender));
        self.input.lock().unwrap().clear();
        self.draw();
        match tokio::time::timeout(Duration::from_secs(60 * 30), receiver).await {
            Ok(Ok(value)) => Ok(value),
            _ => Err(anyhow!("pending input cancelled or timed out")),
        }
    }

    /// Enter the alternate screen, draw, and drive the loop until Ctrl-C.
    pub async fn start(&self, ui: Arc<TerminalUi>) -> AnyResult<()> {
        terminal::enable_raw_mode().ok();
        crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen).ok();
        print!("{CURSOR_HIDE}{MOUSE_ENABLE}");
        let _ = std::io::stdout().flush();
        self.draw();

        // Input thread reads tty bytes; the main loop drives redraw pumps.
        let input_handle = std::thread::spawn({
            let ui = ui.clone();
            let quit = self.quit.clone();
            move || {
                let Ok(mut tty) = std::fs::OpenOptions::new().read(true).open("/dev/tty") else { return };
                let mut buffer = [0u8; 1024];
                loop {
                    let read = match std::io::Read::read(&mut tty, &mut buffer) {
                        Ok(0) => break,
                        Ok(read) => read,
                        Err(_) => break,
                    };
                    let chunk = String::from_utf8_lossy(&buffer[..read]).to_string();
                    ui.on_input(&chunk);
                    if quit.load(Ordering::SeqCst) {
                        break;
                    }
                }
            }
        });

        while !self.quit.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
        input_handle.join().ok();
        terminal::disable_raw_mode().ok();
        print!("{CURSOR_SHOW}{MOUSE_DISABLE}\u{1b}[?1049l");
        let _ = std::io::stdout().flush();
        Ok(())
    }

    pub(crate) fn draw(&self) {
        let transcript = self.transcript.lock().unwrap().clone();
        let input = self.input.lock().unwrap().clone();
        let status = self.status.lock().unwrap().clone();
        let pending = self.pending_input.lock().unwrap().as_ref().map(|(prompt, _)| prompt.clone());
        let scroll_offset = *self.scroll_offset.lock().unwrap();
        let (approval_summary, approval_risk) = {
            let guard = self.approval.lock().unwrap();
            match guard.as_ref() {
                Some((summary, risk, _)) => (Some(summary.clone()), Some(risk.clone())),
                None => (None, None),
            }
        };
        let completion = self.completions.lock().unwrap().first().cloned();
        let conflicts = self.conflicts.lock().unwrap().clone();
        let history = self.cache_history.lock().unwrap().clone();
        let diff = self.diff_view.lock().unwrap().clone();
        let layout = self.workspace.lock().unwrap().clone();
        let values = self.workspace_values.lock().unwrap().clone();
        let plan = self.cache.lock().unwrap().clone();
        let usage = self.cache_usage.lock().unwrap().clone();
        let (width, height) = terminal::size().map(|(width, height)| (width as usize, height as usize)).unwrap_or((80, 24));

        let mut widget_values: Vec<(String, String)> = values;
        if widget_values.is_empty() && layout.is_some() {
            for id in ["cache", "session", "context", "model", "git", "jobs", "mcp"] {
                widget_values.push((id.to_string(), "No data".to_string()));
            }
        }
        if let Some(plan) = &plan {
            let hit = usage.as_ref().map(|usage| if usage.prompt_tokens > 0 { usage.cached_tokens * 100 / usage.prompt_tokens } else { 0 }).unwrap_or(0);
            let value = format!(
                "hash {} · {} tok{}",
                &plan.prefix_hash[..8.min(plan.prefix_hash.len())],
                plan.estimated_prefix_tokens,
                usage.as_ref().map(|_| format!(" · hit {hit}%")).unwrap_or_default()
            );
            let existing = widget_values.iter_mut().find(|(key, _)| key == "cache");
            match existing {
                Some(slot) => slot.1 = value,
                None => widget_values.insert(0, ("cache".to_string(), value)),
            }
        }
        let render_values: Vec<(&str, String)> = widget_values.iter().map(|(key, value)| (key.as_str(), value.clone())).collect();
        let workspace_render = layout.as_ref().map(|layout| WorkspaceRenderState { layout, values: render_values });

        let input_value = pending.as_ref().map(|prompt| format!("{prompt} {input}")).unwrap_or_else(|| input.clone());
        let status_value = if pending.is_some() { "awaiting input · /cancel to abort".to_string() } else { status.clone() };
        let dashboard = render::render_dashboard(
            &DashboardState {
                title: "Harness",
                transcript: &transcript,
                input: &input_value,
                status: &status_value,
                secret_mode: self.secret_mode.load(Ordering::SeqCst),
                cache_plan: plan.as_ref(),
                cache_usage: usage.as_ref(),
                cache_history: history.as_ref(),
                conflicts: conflicts.as_deref(),
                working: self.working.load(Ordering::SeqCst),
                work_frame: self.work_frame.load(Ordering::SeqCst),
                scroll_offset,
                approval: approval_summary.as_deref().and(Some((approval_summary.as_deref().unwrap_or(""), approval_risk.as_deref().unwrap_or("")))),
                completion: completion.as_deref(),
                workspace: workspace_render.as_ref(),
                diff: diff.as_ref(),
            },
            width,
            height,
        );
        let mut stdout = std::io::stdout().lock();
        let _ = write!(stdout, "{ESC}H{dashboard}{ERASE_ROW}{ESC}J");
        let _ = stdout.flush();
    }

    /// Byte-level key handling; port of the TS `onData`.
    pub(crate) fn on_input(&self, chunk: &str) {
        if let Some((button, x, y)) = parse_mouse(chunk) {
            if self.workspace.lock().unwrap().is_some() {
                self.handle_mouse(x, y, button);
                return;
            }
        }
        if chunk == "\u{17}" && self.workspace.lock().unwrap().is_some() {
            if self.visible_widget_count() > 0 {
                let next = (*self.selected_widget.lock().unwrap() + 1) % self.visible_widget_count();
                *self.selected_widget.lock().unwrap() = next;
                self.draw();
            }
            return;
        }
        if chunk == "\u{1b}[1;5C" && self.workspace.lock().unwrap().is_some() {
            *self.widget_mode.lock().unwrap() = Some(WidgetMode::Move);
            self.draw();
            return;
        }
        if chunk == "\u{1b}[1;5D" && self.workspace.lock().unwrap().is_some() {
            *self.widget_mode.lock().unwrap() = Some(WidgetMode::Resize);
            self.draw();
            return;
        }
        if self.widget_mode.lock().unwrap().is_some() && matches!(chunk, "\u{1b}[A" | "\u{1b}[B" | "\u{1b}[C" | "\u{1b}[D") {
            self.move_selected(chunk);
            return;
        }
        if chunk == "\u{3}" {
            self.quit.store(true, Ordering::SeqCst);
            let _ = self.intents.send(Intent::Quit);
            return;
        }
        if self.approval.lock().unwrap().is_some() {
            if matches!(chunk, "y" | "a" | "n" | "Y" | "A" | "N") {
                let resolved = self.approval.lock().unwrap().take();
                if let Some((_, _, sender)) = resolved {
                    let _ = sender.send(!chunk.eq_ignore_ascii_case("n"));
                }
                self.draw();
            }
            return;
        }
        if chunk == ESC {
            if self.widget_mode.lock().unwrap().is_some() {
                *self.widget_mode.lock().unwrap() = None;
                self.draw();
                return;
            }
            if self.pending_input_is_some() {
                let pending = self.pending_input.lock().unwrap().take();
                if let Some((_, sender)) = pending {
                    let _ = sender.send(None);
                }
                self.draw();
                return;
            }
            let _ = self.intents.send(Intent::Cancel);
            return;
        }
        if chunk == "\u{1b}\r" || chunk == "\n" {
            self.input.lock().unwrap().push('\n');
            self.draw();
            return;
        }
        if chunk == "\r" {
            let text = self.input.lock().unwrap().clone();
            if self.pending_input_is_some() {
                let pending = self.pending_input.lock().unwrap().take();
                if let Some((_, sender)) = pending {
                    let answer = if text.starts_with("/cancel") { None } else { Some(text.clone()) };
                    let _ = sender.send(answer);
                }
                self.draw();
                return;
            }
            let trimmed = text.trim().to_string();
            if !trimmed.is_empty() {
                let mut history = self.history.lock().unwrap();
                history.retain(|item| item != &text);
                history.insert(0, text.clone());
                history.truncate(100);
                *self.history_index.lock().unwrap() = -1;
                *self.input.lock().unwrap() = String::new();
                drop(history);
                *self.scroll_offset.lock().unwrap() = 0;
                let _ = self.intents.send(Intent::Submit(text));
            } else {
                self.input.lock().unwrap().clear();
            }
            self.draw();
            return;
        }
        if chunk == "\u{1b}[A" {
            let history = self.history.lock().unwrap().clone();
            let mut index = self.history_index.lock().unwrap();
            if !history.is_empty() && *index < history.len() as isize - 1 {
                *index += 1;
                let value = history[*index as usize].clone();
                *self.input.lock().unwrap() = value;
                drop(index);
                self.draw();
            }
            return;
        }
        if chunk == "\u{1b}[B" {
            let history = self.history.lock().unwrap().clone();
            let mut index = self.history_index.lock().unwrap();
            if *index >= 0 {
                *index -= 1;
                let value = if *index < 0 { String::new() } else { history[*index as usize].clone() };
                *self.input.lock().unwrap() = value;
                drop(index);
                self.draw();
            }
            return;
        }
        if chunk == "\t" && self.input.lock().unwrap().starts_with('/') {
            let entered = self.input.lock().unwrap().clone();
            let candidates: Vec<String> = self.completions.lock().unwrap().iter().filter(|value| value.starts_with(&entered)).cloned().collect();
            if let Some(first) = candidates.first() {
                *self.input.lock().unwrap() = format!("{first}{}", if candidates.len() == 1 { " " } else { "" });
                self.draw();
            }
            return;
        }
        if chunk == "\u{1b}[5~" {
            *self.scroll_offset.lock().unwrap() += 5;
            self.draw();
            return;
        }
        if chunk == "\u{1b}[6~" {
            let mut offset = self.scroll_offset.lock().unwrap();
            *offset = offset.saturating_sub(5);
            drop(offset);
            self.draw();
            return;
        }
        if chunk == "\u{7f}" {
            self.input.lock().unwrap().pop();
            self.draw();
            return;
        }
        if chunk.starts_with(ESC) {
            return;
        }
        self.input.lock().unwrap().push_str(chunk);
        self.draw();
    }

    fn pending_input_is_some(&self) -> bool {
        self.pending_input.lock().unwrap().is_some()
    }

    fn visible_widget_count(&self) -> usize {
        self.workspace
            .lock()
            .unwrap()
            .as_ref()
            .map(|layout| layout.widgets.iter().filter(|widget| widget.visible).count())
            .unwrap_or(0)
    }

    fn move_selected(&self, chunk: &str) {
        let Some(layout) = self.workspace.lock().unwrap().clone() else { return };
        let visible: Vec<_> = layout.widgets.iter().filter(|widget| widget.visible).cloned().collect();
        let selected = *self.selected_widget.lock().unwrap();
        let Some(widget) = visible.get(selected).cloned() else { return };
        let mode = *self.widget_mode.lock().unwrap();
        let dx: isize = match chunk {
            "\u{1b}[C" => 2,
            "\u{1b}[D" => -2,
            _ => 0,
        };
        let dy: isize = match chunk {
            "\u{1b}[B" => 1,
            "\u{1b}[A" => -1,
            _ => 0,
        };
        match mode {
            Some(WidgetMode::Move) => self.update_widget(&widget.id, widget.x as isize + dx, widget.y as isize + dy, widget.width as isize, widget.height as isize),
            Some(WidgetMode::Resize) => self.update_widget(&widget.id, widget.x as isize, widget.y as isize, widget.width as isize + dx, widget.height as isize + dy),
            None => {}
        }
    }

    fn handle_mouse(&self, screen_x: u16, screen_y: u16, release: bool) {
        let Some(layout) = self.workspace.lock().unwrap().clone() else { return };
        let columns = terminal::size().map(|(width, _)| width as usize).unwrap_or(80);
        let x = ((screen_x.saturating_sub(1) as f64) / columns.max(1) as f64 * 100.0).round() as usize;
        let y = screen_y.saturating_sub(3) as usize;
        if release {
            let drag = self.dragging.lock().unwrap().take();
            if let Some((id, _start_x, _start_y, origin_x, origin_y)) = drag {
                self.update_widget(&id, origin_x as isize, origin_y as isize, layout.widgets.iter().find(|candidate| candidate.id == id).map(|widget| widget.width as isize).unwrap_or(30), layout.widgets.iter().find(|candidate| candidate.id == id).map(|widget| widget.height as isize).unwrap_or(3));
            }
            return;
        }
        if let Some(widget) = widget_at(&layout, x, y) {
            let position = layout.widgets.iter().filter(|candidate| candidate.visible).position(|candidate| candidate.id == widget.id).unwrap_or(0);
            *self.selected_widget.lock().unwrap() = position;
            *self.dragging.lock().unwrap() = Some((widget.id.clone(), x, y, widget.x, widget.y));
        }
    }
}

/// Parse `\u{1b}[<button>;<x>;<y>M|m` on a raw chunk; None for other input.
fn parse_mouse(chunk: &str) -> Option<(bool, u16, u16)> {
    let rest = chunk.strip_prefix("\u{1b}[<")?;
    let release = rest.ends_with('m');
    let numbers = rest.trim_end_matches(['M', 'm']);
    let mut parts = numbers.split(';');
    let button = parts.next()?.parse::<u16>().ok()?;
    let x = parts.next()?.parse::<u16>().ok()?;
    let y = parts.next()?.parse::<u16>().ok()?;
    Some(((button & 0b11) != 0 || release, x, y))
}

#[allow(dead_code)]
fn unused(value: HashMap<String, String>) -> usize {
    value.len()
}
