//! Widget workspace, ported from `src/tui/workspace.ts`.

use serde::{Deserialize, Serialize};

pub const WIDGET_IDS: [&str; 10] = ["cache", "session", "context", "model", "git", "jobs", "mcp", "project", "notes", "environment"];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WidgetLayout {
    #[serde(rename = "id")]
    pub id: String,
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
    pub visible: bool,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkspaceLayout {
    pub version: u8,
    pub widgets: Vec<WidgetLayout>,
}

pub fn default_workspace() -> WorkspaceLayout {
    WorkspaceLayout {
        version: 1,
        widgets: [
            ("cache", 0, 0, 34, 3),
            ("session", 34, 0, 33, 3),
            ("context", 67, 0, 33, 3),
            ("model", 0, 3, 50, 3),
            ("git", 50, 3, 50, 3),
            ("jobs", 0, 6, 50, 3),
            ("mcp", 50, 6, 50, 3),
        ]
        .iter()
        .map(|(id, x, y, width, height)| WidgetLayout { id: id.to_string(), x: *x, y: *y, width: *width, height: *height, visible: true })
        .collect(),
    }
}

fn clamp(value: f64, min: usize, max: usize, fallback: usize) -> usize {
    if value.is_finite() {
        ((value.round() as i64).clamp(min as i64, max as i64)) as usize
    } else {
        fallback
    }
}

pub fn normalize_workspace(value: &serde_json::Value) -> WorkspaceLayout {
    let fallback = default_workspace();
    let Some(widgets) = value.get("widgets").and_then(serde_json::Value::as_array) else { return fallback };
    if value.get("version").and_then(serde_json::Value::as_u64) != Some(1) {
        return fallback;
    }
    let mut seen = std::collections::HashSet::new();
    let mut normalized: Vec<WidgetLayout> = Vec::new();
    for item in widgets {
        let Some(id) = item.get("id").and_then(serde_json::Value::as_str) else { continue };
        if !WIDGET_IDS.contains(&id) || seen.contains(id) {
            continue;
        }
        let number = |key: &str| item.get(key).and_then(serde_json::Value::as_f64).unwrap_or(f64::NAN);
        let width = clamp(number("width"), 10, 100, 30);
        let height = clamp(number("height"), 2, 20, 3);
        seen.insert(id.to_string());
        normalized.push(WidgetLayout {
            id: id.to_string(),
            x: clamp(number("x"), 0, 100 - width, 0),
            y: clamp(number("y"), 0, 100 - height, 0),
            width,
            height,
            visible: item.get("visible").and_then(serde_json::Value::as_bool).unwrap_or(true),
        });
    }
    for widget in &fallback.widgets {
        if !seen.contains(&widget.id) {
            normalized.push(widget.clone());
        }
    }
    WorkspaceLayout { version: 1, widgets: normalized }
}

fn workspace_path(pi_config_dir: &std::path::Path, cwd: &std::path::Path, project: bool) -> std::path::PathBuf {
    if project {
        cwd.join(".harness").join("workspace.json")
    } else {
        pi_config_dir.join("workspace.json")
    }
}

pub fn load_workspace(pi_config_dir: &std::path::Path, cwd: &std::path::Path, project_trusted: bool) -> WorkspaceLayout {
    let candidates = if project_trusted { vec![workspace_path(pi_config_dir, cwd, true), workspace_path(pi_config_dir, cwd, false)] } else { vec![workspace_path(pi_config_dir, cwd, false)] };
    let mut layouts = candidates.into_iter().filter_map(|path| std::fs::read_to_string(path).ok());
    match layouts.next() {
        Some(content) => serde_json::from_str::<serde_json::Value>(&content).map(|value| normalize_workspace(&value)).unwrap_or_else(|_| default_workspace()),
        None => default_workspace(),
    }
}

pub fn has_project_workspace(cwd: &std::path::Path) -> bool {
    cwd.join(".harness").join("workspace.json").exists()
}

pub fn save_workspace(pi_config_dir: &std::path::Path, cwd: &std::path::Path, project: bool, layout: &WorkspaceLayout) -> Result<(), Box<dyn std::error::Error>> {
    let path = workspace_path(pi_config_dir, cwd, project);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let normalized = normalize_workspace(&serde_json::to_value(layout)?);
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = std::fs::OpenOptions::new().create(true).write(true).truncate(true).mode(0o600).open(&path)?;
        file.write_all(serde_json::to_string_pretty(&normalized)?.as_bytes())?;
    }
    #[cfg(not(unix))]
    std::fs::write(&path, serde_json::to_string_pretty(&normalized)?)?;
    Ok(())
}

pub fn workspace_bounds(layout: &WorkspaceLayout) -> (usize, usize) {
    let width = layout.widgets.iter().map(|widget| widget.x + widget.width).max().unwrap_or(100).max(100);
    let height = layout.widgets.iter().map(|widget| widget.y + widget.height).max().unwrap_or(3).max(3);
    (width, height)
}

pub fn widget_at(layout: &WorkspaceLayout, x: usize, y: usize) -> Option<&WidgetLayout> {
    layout.widgets.iter().find(|widget| widget.visible && x >= widget.x && x < widget.x + widget.width && y >= widget.y && y < widget.y + widget.height)
}
