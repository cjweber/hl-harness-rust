//! Themes, ported from `src/tui/theme.ts`.

use serde::{Deserialize, Serialize};

pub const BACKGROUND: &str = "#111827";
pub const FOREGROUND: &str = "#e5e7eb";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeColors {
    pub accent: String,
    pub muted: String,
    pub success: String,
    pub warning: String,
    pub error: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Theme {
    pub name: &'static str,
    pub colors: ThemeColors,
}

pub fn theme_static(name: &'static str, accent: &'static str, muted: &'static str, success: &'static str, warning: &'static str, error: &'static str) -> Theme {
    Theme { name, colors: ThemeColors { accent: accent.into(), muted: muted.into(), success: success.into(), warning: warning.into(), error: error.into() } }
}

pub fn whimsical() -> Theme {
    theme_static("whimsical", "#ff70a6", "#7c6f9f", "#5ec4a6", "#ffb347", "#ef476f")
}

pub fn default_theme() -> Theme {
    theme_static("default", "#67e8f9", "#9ca3af", "#86efac", "#fcd34d", "#fca5a5")
}

pub fn themes() -> [Theme; 2] {
    [default_theme(), whimsical()]
}

pub static ACTIVE: std::sync::Mutex<Option<ThemeColors>> = std::sync::Mutex::new(None);

/// Currently selected colors: the runtime override set from settings + resource
/// theme JSONs, else the whimsical default (mirrors the TS whimsical styling).
pub fn current_colors() -> ThemeColors {
    ACTIVE.lock().unwrap().clone().unwrap_or_else(|| whimsical().colors)
}

/// Resolve a runtime theme from a theme JSON file
/// ({"name": "...", "accent": "#..", "muted": "#..", "success": "#..", "warning": "#..", "error": "#.."}).
pub fn theme_from_json(value: &str) -> Option<ThemeColors> {
    let parsed: serde_json::Value = serde_json::from_str(value).ok()?;
    let color = |key: &str| parsed[key].as_str().filter(|value| value.starts_with('#')).map(|value| value.to_string());
    Some(ThemeColors {
        accent: color("accent")?,
        muted: color("muted")?,
        success: color("success")?,
        warning: color("warning")?,
        error: color("error")?,
    })
}

pub fn set_active(theme: ThemeColors) {
    *ACTIVE.lock().unwrap() = Some(theme);
}
