
use hl_harness::tui::render::{
    render_dashboard, render_diff_view, render_markdown, render_transcript, wrap_text, CacheHistory, CacheUsageView,
    DashboardState, ERASE_ROW,
};
use hl_harness::tui::theme::themes;
use hl_harness::tui::workspace::{default_workspace, normalize_workspace, widget_at};

#[test]
fn wrap_preserves_content_within_visible_width() {
    let text = "alpha beta gamma delta epsilon zeta eta theta";
    let wrapped = wrap_text(text, 20, "");
    assert!(wrapped.len() > 1);
    assert!(wrapped.iter().all(|line| line.chars().count() <= 20));
    assert_eq!(wrapped.join(" ").replace("  ", " ").trim(), text);
}

#[test]
fn transcript_styles_roles_groups_and_caps() {
    let long = (0..100).map(|index| format!("line {index}")).collect::<Vec<_>>().join("\n");
    let out = render_transcript(
        &["[user] long request".to_string(), "● assistant: uses **bold**".to_string(), "▣ tool bash {\"command\":\"cat\"}\n```json\nlong\n```".to_string(), format!("▣ tool bash: ok\n```json\n{long}\n```")],
        60,
        &themes()[1].colors,
    );
    let joined = out.join("\n");
    assert!(joined.contains("More lines truncated") || joined.contains("more lines"), "{joined}");
    assert!(joined.contains("long request"));
}

#[test]
fn markdown_renders_headings_bullets_and_diff() {
    let rendered = render_markdown("# Title\n- one\n- two\n> quoted\n");
    assert!(rendered.contains("◆ Title"), "{rendered}");
    assert!(rendered.contains("one"));
    let diff_block = render_markdown("```diff\n-a\n+b\n```");
    assert!(diff_block.contains('a') || diff_block.contains('b'), "{diff_block}");
}

#[test]
fn dashboard_rows_fit_and_terminate() {
    let usage = CacheUsageView { prompt_tokens: 200, cached_tokens: 50, requests: 3 };
    let history = CacheHistory::default();
    let screen = render_dashboard(
        &DashboardState {
            title: "Harness",
            transcript: &["› you long text wrapping multiple times".to_string(), "● assistant: reply".to_string()],
            input: "typing",
            status: "Ready",
            quote: "the diff is the truth",
            secret_mode: false,
            cache_plan: None,
            cache_usage: Some(&usage),
            cache_history: Some(&history),
            conflicts: None,
            working: true,
            work_frame: 0,
            scroll_offset: 0,
            approval: Some(("read: x", "safe")),
            completion: None,
            workspace: None,
            diff: None,
        },
        40,
        40,
    );
    for row in screen.split("\r\n") {
        let visible = strip_ansi(row);
        assert!(visible.chars().count() <= 40, "row too wide: {row:?} ({visible:?})");
        assert!(row.ends_with(ERASE_ROW), "row missing erase: {row:?}");
    }
    let visible = strip_ansi(&screen);
    assert!(visible.contains("Harness"), "{visible:?}");
    assert!(visible.contains("✦"), "gradient banner: {visible:?}");
    assert!(visible.matches("y allow").count() == 1);
    assert!(visible.contains("the diff is the truth"), "quote line: {visible:?}");
}

fn strip_ansi(row: &str) -> String {
    let mut out = String::new();
    let mut chars = row.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\u{1b}' {
            for follow in chars.by_ref() {
                if follow == 'm' || follow == 'K' || follow == 'H' || follow == 'J' {
                    break;
                }
            }
        } else {
            out.push(ch);
        }
    }
    out
}

#[test]
fn workspace_normalizes_and_hits() {
    let layout = default_workspace();
    let (width, height) = hl_harness::tui::workspace::workspace_bounds(&layout);
    assert!(width >= 100 && height >= 9, "{width}x{height}");
    assert_eq!(widget_at(&layout, 34, 0).unwrap().id, "session");
    let raw = serde_json::json!({"version": 1, "widgets": [{"id": "cache", "y": 50, "x": 999, "width": 3, "negative": true}]});
    let normalized = normalize_workspace(&raw);
    assert!(normalized.widgets.iter().any(|widget| widget.id == "cache" && widget.x < 100));
}

#[test]
fn diff_navigation_state_transitions() {
    let mut view = hl_harness::tui::diff_view::parse_diff("diff --git a/x b/x\n--- a/x\n+++ b/x\n@@ -1 +1 @@\n-old\n+new\n");
    assert_eq!(view.files.len(), 1);
    let (file, hunk) = hl_harness::tui::diff_view::selected_diff(&view);
    assert_eq!(file.unwrap().path, "x");
    assert_eq!(hunk.unwrap().lines.len(), 2);
    view.hunk_index = 5;
    // Selection out of range returns None safely
    let (_, hunk) = hl_harness::tui::diff_view::selected_diff(&view);
    assert!(hunk.is_none());
    let _ = render_diff_view(&view);
}
