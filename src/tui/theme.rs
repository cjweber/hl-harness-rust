//! Themes, ported from `src/tui/theme.ts`.

use serde::{Deserialize, Serialize};

pub const BACKGROUND: &str = "#111827";
pub const FOREGROUND: &str = "#e5e7eb";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ThemeColors {
    pub accent: &'static str,
    pub muted: &'static str,
    pub success: &'static str,
    pub warning: &'static str,
    pub error: &'static str,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Theme {
    pub name: &'static str,
    pub colors: ThemeColors,
}

pub const WHIMSICAL: Theme = Theme {
    name: "whimsical",
    colors: ThemeColors {
        accent: "#ff70a6",
        muted: "#7c6f9f",
        success: "#5ec4a6",
        warning: "#ffb347",
        error: "#ef476f",
    },
};

pub const THEMES: [Theme; 2] = [
    Theme { name: "default", colors: ThemeColors { accent: "#67e8f9", muted: "#9ca3af", success: "#86efac", warning: "#fcd34d", error: "#fca5a5" } },
    WHIMSICAL,
];
