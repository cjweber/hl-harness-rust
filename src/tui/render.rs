//! Pure render functions, ported from the renderer half of `src/tui/screen.ts`
//! (dashboard, transcript formatting, wrapping, tool cards, monitors, widgets).

use super::diff_view::{parse_diff, selected_diff, DiffView};
use super::theme::{current_colors, ThemeColors};
use crate::types::CachePlan;

pub const ESC: &str = "\u{1b}[";
pub const CURSOR_HIDE: &str = "\u{1b}[?25l";
pub const ERASE_ROW: &str = "\u{1b}[K";

#[derive(Clone, Debug)]
pub enum Unit {
    Escape(String),
    Char(char),
}

/// Split a styled string into grapheme/escape units; escape sequences are atomic.
pub fn scan_units(value: &str) -> Vec<Unit> {
    let chars: Vec<char> = value.chars().collect();
    let mut units = Vec::new();
    let mut index = 0usize;
    while index < chars.len() {
        if chars[index] == '\u{1b}' {
            let mut sequence = String::new();
            while index < chars.len() {
                sequence.push(chars[index]);
                let ch = chars[index];
                index += 1;
                if ch == 'm' {
                    break;
                }
            }
            units.push(Unit::Escape(sequence));
        } else {
            units.push(Unit::Char(chars[index]));
            index += 1;
        }
    }
    units
}

pub fn visible_length(value: &str) -> usize {
    scan_units(value).iter().filter(|unit| matches!(unit, Unit::Char(_))).count()
}

pub fn color(value: &str, hex: &str) -> String {
    let [r, g, b] = hex_rgb(hex);
    format!("{ESC}38;2;{r};{g};{b}m{value}{ESC}39m")
}

pub fn hex_rgb(hex: &str) -> [u8; 3] {
    [
        u8::from_str_radix(hex.get(1..3).unwrap_or("00"), 16).unwrap_or(0),
        u8::from_str_radix(hex.get(3..5).unwrap_or("00"), 16).unwrap_or(0),
        u8::from_str_radix(hex.get(5..7).unwrap_or("00"), 16).unwrap_or(0),
    ]
}

pub fn ansi_bold(value: &str) -> String {
    format!("{ESC}1m{value}{ESC}22m")
}

pub fn pacman_indicator(frame: usize, colors: &ThemeColors) -> String {
    let ghosts = color("ᕚ", colors.accent.as_str());
    let dots = color("·", colors.warning.as_str());
    match frame % 4 {
        0 => format!("{} {} {} {}", color("ᗧ", colors.warning.as_str()), ghosts, dots, dots),
        1 => format!("{}{} {} {}", color("ᗧ", colors.warning.as_str()), dots, ghosts, dots),
        2 => format!("{} {} {}{}", color("ᗧ", colors.warning.as_str()), dots, ghosts, dots),
        _ => format!("{} {} {}{}", color("ᗧ", colors.warning.as_str()), dots, dots, ghosts),
    }
}

pub fn fit(value: &str, width: usize) -> String {
    if visible_length(value) <= width {
        return value.to_string();
    }
    let mut out = String::new();
    let mut counted = 0usize;
    let has_escape = value.contains('\u{1b}');
    for unit in scan_units(value) {
        match unit {
            Unit::Escape(sequence) => out.push_str(&sequence),
            Unit::Char(ch) => {
                if counted >= width.saturating_sub(1) {
                    break;
                }
                out.push(ch);
                counted += 1;
            }
        }
    }
    if width > 1 {
        format!("{out}{}…", if has_escape { format!("{ESC}39m") } else { String::new() })
    } else {
        "…".to_string()
    }
}

/// Word-wrap a (possibly ANSI-styled) line to a visible width; never splits escape codes.
pub fn wrap_text(text: &str, width: usize, indent: &str) -> Vec<String> {
    let indent_width = visible_length(indent);
    let limit = width.saturating_sub(indent_width).max(8);
    let mut lines: Vec<String> = Vec::new();
    for segment in text.split('\n') {
        if segment.is_empty() {
            lines.push(String::new());
            continue;
        }
        let units = scan_units(segment);
        let mut current = String::new();
        let mut plain_width = 0usize;
        let mut last_break: Option<usize> = None;
        for unit in units {
            if let Unit::Escape(sequence) = &unit {
                current.push_str(sequence);
                continue;
            }
            let Unit::Char(ch) = unit else { continue };
            if plain_width >= limit {
                let cut = last_break.unwrap_or(current.chars().count());
                let head: String = current.chars().take(cut).collect();
                let mut tail: String = current.chars().skip(cut).collect();
                if last_break.is_some() {
                    tail = tail.trim_start_matches(' ').to_string();
                }
                lines.push(format!("{indent}{head}"));
                current = tail;
                plain_width = visible_length(&current);
                last_break = None;
            }
            if ch == ' ' && plain_width > 0 {
                last_break = Some(current.chars().count());
            }
            current.push(ch);
            plain_width += 1;
        }
        lines.push(format!("{indent}{current}"));
    }
    lines
}

const TOOL_BODY_LIMIT: usize = 60;

fn render_entry(entry: &str, width: usize, colors: &ThemeColors) -> Vec<String> {
    if let Some(rest) = entry.strip_prefix("▣ tool ") {
        let (header, body) = match rest.split_once('\n') {
            Some((head, body)) => (head, body),
            None => (rest, ""),
        };
        let name = header.split(':').next().unwrap_or(header).to_string();
        let args = if header.len() > name.len() { header[name.len()..].trim() } else { "" };
        let mut head = color("▣ ", colors.accent.as_str()) + &ansi_bold(&name);
        if !args.is_empty() {
            head.push_str(&color(&format!(" {}", fit(args, width.saturating_sub(name.chars().count() + 6))), colors.muted.as_str()));
        }
        let body_lines: Vec<String> = if body.is_empty() {
            Vec::new()
        } else {
            render_markdown(body).split('\n').flat_map(|line| wrap_text(line, width, "  ")).collect()
        };
        let capped = if body_lines.len() > TOOL_BODY_LIMIT {
            let mut lines = body_lines[..TOOL_BODY_LIMIT].to_vec();
            lines.push(color(&format!("  … {} more lines", body_lines.len() - TOOL_BODY_LIMIT), colors.muted.as_str()));
            lines
        } else {
            body_lines
        };
        return [vec![head], capped].concat();
    }
    if let Some(body) = entry.strip_prefix("assistant: ") {
        return [vec![color(" ● assistant", colors.success.as_str())], wrap_text(&render_markdown(body), width, "  ")].concat();
    }
    if let Some(body) = entry.strip_prefix("> ") {
        return [vec![color(" › you", colors.accent.as_str())], wrap_text(body, width, "  ")].concat();
    }
    if let Some(body) = entry.strip_prefix("error: ") {
        return [vec![color(" ✗ error", colors.error.as_str())], wrap_text(body, width, "  ")].concat();
    }
    if let Some(body) = entry.strip_prefix("system: ") {
        return [vec![color(" · system", colors.muted.as_str())], wrap_text(body, width, "  ")].concat();
    }
    wrap_text(entry, width, "")
}

/// Role styling, spacing, and width-bounded wrapping across transcript entries.
pub fn render_transcript(entries: &[String], width: usize, colors: &ThemeColors) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (index, entry) in entries.iter().enumerate() {
        if index > 0 && !(entries[index - 1].starts_with("▣ ") && entry.starts_with("▣ ")) {
            out.push(String::new());
        }
        out.extend(render_entry(entry, width, colors));
    }
    out
}

pub fn render_diff(value: &str, colors: &ThemeColors) -> String {
    value
        .split('\n')
        .map(|line| {
            if line.starts_with("+++") || line.starts_with("---") || line.starts_with("diff --git") {
                ansi_bold(&color(line, colors.accent.as_str()))
            } else if line.starts_with("@@") {
                color(line, colors.accent.as_str())
            } else if line.starts_with('+') {
                color(line, colors.success.as_str())
            } else if line.starts_with('-') {
                color(line, colors.error.as_str())
            } else if line.starts_with("index ") || line.starts_with("new file") || line.starts_with("deleted file") {
                color(line, colors.muted.as_str())
            } else {
                line.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// Markdown rendering: fenced code blocks (with diff highlighting), headings, bullets, quotes, bold.
pub fn render_markdown(value: &str) -> String {
    let colors = &current_colors();
    let mut blocks: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut inside_fence = false;
    let mut fence_language = String::new();
    let mut fence_body: Vec<String> = Vec::new();
    let flush = |blocks: &mut Vec<String>, current: &mut String| {
        if !current.is_empty() {
            blocks.push(current.clone());
            current.clear();
        }
    };
    for line in value.split('\n') {
        if !inside_fence && line.trim_start().starts_with("```") {
            flush(&mut blocks, &mut current);
            inside_fence = true;
            fence_language = line.trim_start_matches('`').trim().to_string();
            continue;
        }
        if inside_fence && line.trim_start().starts_with("```") {
            let rendered = if fence_language == "diff" {
                render_diff(&fence_body.join("\n"), colors)
            } else {
                fence_body.join("\n")
            };
            blocks.push(rendered);
            inside_fence = false;
            fence_body.clear();
            fence_language.clear();
            continue;
        }
        if inside_fence {
            fence_body.push(line.to_string());
            continue;
        }
        let styled = if let Some(heading) = line.strip_prefix("# ") {
            ansi_bold(&color(&format!("◆ {heading}"), colors.accent.as_str()))
        } else if line.starts_with('#') {
            let heading = line.trim_start_matches('#').trim_start();
            ansi_bold(&color(&format!("◆ {heading}"), colors.accent.as_str()))
        } else if let Some(bullet) = line.strip_prefix("- ") {
            format!("  {} {}", color("•", colors.accent.as_str()), bullet)
        } else if let Some(bullet) = line.strip_prefix("* ") {
            format!("  {} {}", color("•", colors.accent.as_str()), bullet)
        } else if let Some(quote) = line.strip_prefix("> ") {
            color(&format!("│ {quote}"), colors.muted.as_str())
        } else {
            line.to_string()
        };
        current.push_str(&styled);
        current.push('\n');
    }
    blocks.push(current);
    let joined: Vec<String> = blocks.into_iter().filter(|block| !block.is_empty()).collect();
    joined.join("\n").trim_end_matches('\n').to_string()
}

pub fn render_workspace(layout: &super::workspace::WorkspaceLayout, values: &[(&str, String)], width: usize) -> Vec<String> {
    let safe_width = width.max(20);
    let rows = layout.widgets.iter().map(|widget| widget.y + widget.height).max().unwrap_or(1);
    let mut canvas: Vec<Vec<char>> = vec![vec![' '; safe_width]; rows];
    for widget in layout.widgets.iter().filter(|widget| widget.visible) {
        let left = (((widget.x as f64) / 100.0) * safe_width as f64).round() as usize;
        let left = left.min(safe_width - 1);
        let top = widget.y;
        let box_width = widget.width.max(4).min(safe_width - left);
        let title = format!(" {} ", widget.id);
        for row in 0..widget.height.min(rows.saturating_sub(top)) {
            for column in 0..box_width {
                let edge = column == 0 || column == box_width - 1;
                let ch = if row == 0 {
                    if column == 0 { '┌' } else if edge { '┐' } else { '─' }
                } else if row == widget.height - 1 {
                    if column == 0 { '└' } else if edge { '┘' } else { '─' }
                } else if edge {
                    '│'
                } else {
                    ' '
                };
                if left + column < safe_width {
                    canvas[top + row][left + column] = ch;
                }
            }
            if row == 0 {
                for (column, ch) in title.chars().enumerate().take(box_width.saturating_sub(2)) {
                    if left + column + 1 < safe_width {
                        canvas[top][left + column + 1] = ch;
                    }
                }
            }
            if row == 1 {
                let value = values.iter().find(|(key, _)| *key == widget.id).map(|(_, value)| value.as_str()).unwrap_or("No data");
                let content: String = value.chars().take(box_width.saturating_sub(4)).collect();
                for (column, ch) in content.chars().enumerate() {
                    let target = left + column + 1;
                    if target < safe_width && target <= left + box_width - 2 {
                        canvas[top + row][target] = ch;
                    }
                }
            }
        }
    }
    canvas.into_iter().map(|row| row.iter().collect::<String>().trim_end().to_string()).collect()
}

#[derive(Clone, Debug, PartialEq)]
pub struct CacheHistoryEntry {
    pub prefix_hash: String,
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub latency_ms: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct CacheHistory {
    pub entries: Vec<CacheHistoryEntry>,
    pub index: usize,
}

pub fn render_cache_monitor(history: &CacheHistory, page: usize) -> Vec<String> {
    if history.entries.is_empty() {
        return vec!["cache history: empty".to_string(), "/cache close".to_string()];
    }
    let total = history.entries.len();
    let prompt: u64 = history.entries.iter().map(|entry| entry.prompt_tokens).sum();
    let cached: u64 = history.entries.iter().map(|entry| entry.cached_tokens).sum();
    let latencies: Vec<u64> = history.entries.iter().filter_map(|entry| entry.latency_ms).collect();
    let average = if latencies.is_empty() { 0 } else { latencies.iter().sum::<u64>() / latencies.len() as u64 };
    let start = history.index.saturating_sub(page / 2);
    let mut lines = vec![format!(
        "cache history {}/{} · hit {cached}/{prompt} tok ({}%) · avg {average}ms",
        history.index + 1,
        total,
        if prompt > 0 { cached * 100 / prompt } else { 0 }
    )];
    for offset in 0..page.min(total - start) {
        let position = start + offset;
        let entry = &history.entries[position];
        let hit = if entry.prompt_tokens > 0 { entry.cached_tokens * 100 / entry.prompt_tokens } else { 0 };
        lines.push(format!(
            "{} {:>3} {} prompt={} cached={} hit={hit}% {}",
            if position == history.index { '›' } else { ' ' },
            position + 1,
            &entry.prefix_hash[..12.min(entry.prefix_hash.len())],
            entry.prompt_tokens,
            entry.cached_tokens,
            entry.latency_ms.map(|ms| format!("{ms}ms")).unwrap_or_default()
        ));
    }
    lines.push("/cache next|prev · close".to_string());
    lines
}

pub fn render_diff_view(view: &DiffView) -> Vec<String> {
    let (file, hunk) = selected_diff(view);
    let Some(file) = file else { return vec!["diff: no files".to_string()] };
    let colors = &current_colors();
    let hunk_text: Vec<String> = match hunk {
        Some(hunk) => hunk.lines.clone(),
        None => file.lines[..80.min(file.lines.len())].to_vec(),
    };
    let mut lines = vec![format!(
        "diff {}/{} {} · hunk {}/{} · {}",
        view.file_index + 1,
        view.files.len(),
        file.path,
        if hunk.is_some() { view.hunk_index + 1 } else { 0 },
        file.hunks.len(),
        view.mode
    )];
    if view.mode == "split" {
        for index in 0..hunk_text.len() {
            let old_line = hunk_text.get(index).map(|line| if line.starts_with('+') { " ".to_string() } else { line.clone() }).unwrap_or_default();
            let new_line = hunk_text.get(index).map(|line| if line.starts_with('-') { " ".to_string() } else { line.clone() }).unwrap_or_default();
            lines.push(format!("{:<width$} │ {}", old_line, new_line, width = 38));
        }
    } else {
        let target = hunk.map(|hunk| [vec![hunk.header.clone()], hunk.lines.clone()].concat()).unwrap_or_else(|| file.lines.clone());
        lines.extend(target.iter().map(|line| render_diff(line, colors)));
    }
    lines.push("/diff next|prev · file next|prev · hunk next|prev · split|unified · close".to_string());
    lines
}

#[derive(Clone, Debug)]
pub struct CacheUsageView {
    pub prompt_tokens: u64,
    pub cached_tokens: u64,
    pub requests: u64,
}

#[derive(Clone, Debug)]
pub struct WorkspaceRenderState<'a> {
    pub layout: &'a super::workspace::WorkspaceLayout,
    pub values: Vec<(&'a str, String)>,
}

pub struct DashboardState<'a> {
    pub title: &'a str,
    pub transcript: &'a [String],
    pub input: &'a str,
    pub status: &'a str,
    pub secret_mode: bool,
    pub cache_plan: Option<&'a CachePlan>,
    pub cache_usage: Option<&'a CacheUsageView>,
    pub cache_history: Option<&'a CacheHistory>,
    pub conflicts: Option<&'a [String]>,
    pub working: bool,
    pub work_frame: usize,
    pub scroll_offset: usize,
    pub approval: Option<(&'a str, &'a str)>,
    pub completion: Option<&'a str>,
    pub workspace: Option<&'a WorkspaceRenderState<'a>>,
    pub diff: Option<&'a DiffView>,
}

pub fn render_dashboard(state: &DashboardState<'_>, width: usize, height: usize) -> String {
    let colors = &current_colors();
    let safe_width = width.max(20);
    let safe_height = height.max(8);
    let cache_line: Option<String> = state.cache_plan.as_ref().map(|plan| {
        let hash = &plan.prefix_hash[..8.min(plan.prefix_hash.len())];
        match state.cache_usage {
            Some(usage) => format!(
                "cache {hash} · {} tok · hit {}/{} tok ({}%) · {} req",
                plan.estimated_prefix_tokens,
                usage.cached_tokens,
                usage.prompt_tokens,
                if usage.prompt_tokens > 0 { usage.cached_tokens * 100 / usage.prompt_tokens } else { 0 },
                usage.requests
            ),
            None => format!("cache {hash} · {} tok", plan.estimated_prefix_tokens),
        }
    });
    let mut lines: Vec<String> = Vec::new();
    lines.push(fit(&format!(" {} ", state.title), safe_width));
    lines.push(fit(&cache_line.map(|line| format!(" {line}")).unwrap_or_else(|| " cache unavailable".into()), safe_width));
    if let Some(workspace) = state.workspace {
        lines.extend(render_workspace(workspace.layout, &workspace.values, safe_width));
    }
    if let Some(conflicts) = state.conflicts {
        lines.push("conflicts:".to_string());
        lines.extend(conflicts.iter().cloned());
    }
    if let Some(history) = state.cache_history {
        lines.extend(render_cache_monitor(history, 8));
    }
    if let Some(diff) = state.diff {
        lines.extend(render_diff_view(diff));
    }
    lines.push(String::new());
    lines.extend(render_transcript(state.transcript, safe_width, colors));
    if let Some((risk, summary)) = state.approval {
        lines.push(String::new());
        lines.push(color(&format!("APPROVAL REQUIRED [{risk}] {summary}"), colors.warning.as_str()));
        lines.push(" y allow · a allow always · n deny".to_string());
    }
    lines.push(String::new());
    lines.push(fit(&if state.secret_mode { "> ".to_string() } else { format!("> {}", state.input) }, safe_width));
    lines.push(fit(&format!(" {}{}", state.status, if state.working { format!("  {}", pacman_indicator(state.work_frame, colors)) } else { String::new() }), safe_width));
    lines.push(fit(" Enter send · /attach image · /compact · /tree · /branch id · PgUp/PgDn scroll · Ctrl-C quit", safe_width));
    let end = lines.len().saturating_sub(state.scroll_offset);
    let begin = end.saturating_sub(safe_height);
    let begin = begin.min(lines.len());
    let end = end.min(lines.len());
    let visible: Vec<String> = lines[begin..end].iter().map(|line| format!("{}{ERASE_ROW}", fit(line, safe_width))).collect();
    format!("{CURSOR_HIDE}{}", visible.join("\r\n"))
}

pub fn parse_diff_view(value: &str) -> DiffView {
    parse_diff(value)
}
