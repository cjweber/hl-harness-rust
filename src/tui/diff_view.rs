//! Diff parsing/navigation state, ported from `src/tui/diff.ts`.

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffHunk {
    pub header: String,
    pub lines: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffFile {
    pub path: String,
    pub lines: Vec<String>,
    pub hunks: Vec<DiffHunk>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiffView {
    pub files: Vec<DiffFile>,
    pub file_index: usize,
    pub hunk_index: usize,
    pub mode: String, // "unified" | "split"
}

pub fn parse_diff(value: &str) -> DiffView {
    let mut files: Vec<DiffFile> = Vec::new();
    let mut current: Option<usize> = None;
    let trimmed = value.trim_end_matches('\n');
    for line in trimmed.split('\n') {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            let path = rest
                .strip_prefix("a/")
                .unwrap_or(rest)
                .split(" b/")
                .next()
                .unwrap_or(rest)
                .to_string();
            files.push(DiffFile { path, lines: Vec::new(), hunks: Vec::new() });
            current = Some(files.len() - 1);
            continue;
        }
        if current.is_none() {
            continue;
        }
        let index = current.unwrap();
        let hunk_index = files[index].hunks.len().saturating_sub(1);
        files[index].lines.push(line.to_string());
        if line.starts_with("@@") {
            files[index].hunks.push(DiffHunk { header: line.to_string(), lines: Vec::new() });
        } else if files[index].hunks.is_empty() {
            // Pre-hunk metadata lines land on the file but never on a hunk.
            let _ = hunk_index;
        } else {
            files[index].hunks.last_mut().unwrap().lines.push(line.to_string());
        }
    }
    DiffView { files, file_index: 0, hunk_index: 0, mode: "unified".to_string() }
}

pub fn selected_diff(view: &DiffView) -> (Option<&DiffFile>, Option<&DiffHunk>) {
    let file = view.files.get(view.file_index);
    (file, file.and_then(|file| file.hunks.get(view.hunk_index)))
}
