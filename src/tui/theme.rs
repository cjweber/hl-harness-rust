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

// ---------------------------------------------------------------------------
// Bubbly future-wave styling: per-section hues, gradients, soft shading.
// ---------------------------------------------------------------------------

/// One hue per workspace section / tool family.
pub fn section_color(section: &str) -> &'static str {
    match section {
        "cache" => "#22d3ee",       // cyan
        "session" => "#a78bfa",     // violet
        "context" => "#38bdf8",     // sky
        "model" => "#e879f9",       // fuchsia
        "git" => "#fb923c",         // orange
        "jobs" => "#fbbf24",        // amber
        "mcp" => "#f472b6",         // pink
        "project" => "#34d399",     // emerald
        "notes" => "#facc15",       // yellow
        "environment" => "#94a3b8", // slate
        _ => "#67e8f9",
    }
}

pub fn hex_rgb(hex: &str) -> [u8; 3] {
    [
        u8::from_str_radix(hex.get(1..3).unwrap_or("00"), 16).unwrap_or(0),
        u8::from_str_radix(hex.get(3..5).unwrap_or("00"), 16).unwrap_or(0),
        u8::from_str_radix(hex.get(5..7).unwrap_or("00"), 16).unwrap_or(0),
    ]
}

fn parse_rgb(hex: &str) -> [f64; 3] {
    let rgb = hex_rgb(hex);
    [rgb[0] as f64, rgb[1] as f64, rgb[2] as f64]
}

fn to_hex(rgb: [f64; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb[0].round() as u8, rgb[1].round() as u8, rgb[2].round() as u8)
}

/// Blend toward white by `amount` (0..1) — the soft "glow" of the wave style.
pub fn lighten(hex: &str, amount: f64) -> String {
    let rgb = parse_rgb(hex);
    let mixed = [
        rgb[0] + (255.0 - rgb[0]) * amount,
        rgb[1] + (255.0 - rgb[1]) * amount,
        rgb[2] + (255.0 - rgb[2]) * amount,
    ];
    to_hex(mixed)
}

/// Blend toward the background by `amount` (0..1) — the dimmed shading pass.
pub fn darken(hex: &str, amount: f64) -> String {
    let rgb = parse_rgb(hex);
    let mixed = [rgb[0] * (1.0 - amount), rgb[1] * (1.0 - amount), rgb[2] * (1.0 - amount)];
    to_hex(mixed)
}

/// Interpolate two colors across `text`, one span per character.
pub fn color(value: &str, hex: &str) -> String {
    let [r, g, b] = hex_rgb(hex);
    format!("\u{1b}[38;2;{r};{g};{b}m{value}\u{1b}[39m")
}

pub fn gradient_text(text: &str, from: &str, to: &str) -> String {
    let start = parse_rgb(from);
    let end = parse_rgb(to);
    let count = text.chars().count().max(1);
    let mut out = String::new();
    for (index, ch) in text.chars().enumerate() {
        let t = index as f64 / (count.saturating_sub(1)).max(1) as f64;
        let mixed = [
            start[0] + (end[0] - start[0]) * t,
            start[1] + (end[1] - start[1]) * t,
            start[2] + (end[2] - start[2]) * t,
        ];
        out.push_str(&color(&ch.to_string(), &to_hex(mixed)));
    }
    out
}

/// A soft wave-shaded run of `cells` blocks from `from` to `to`.
pub fn gradient_bar(cells: usize, from: &str, to: &str) -> String {
    gradient_text(&"▮".repeat(cells), from, to)
}
