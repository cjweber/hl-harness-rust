//! Resource loader, ported from `src/resources/loader.ts`: conventional
//! directories, `package.json` `pi` manifests with glob matching, frontmatter,
//! context files, system prompt assembly, and prompt/skill expansion.

use super::registry::{ResourceEntry, ResourceKind, ResourceRegistry};
use anyhow::{anyhow, bail, Result};
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

const CONVENTIONAL: [(ResourceKind, &str); 4] = [
    (ResourceKind::Extension, "extensions"),
    (ResourceKind::Skill, "skills"),
    (ResourceKind::Prompt, "prompts"),
    (ResourceKind::Theme, "themes"),
];

const EXTENSION_EXTS: [&str; 4] = [".ts", ".js", ".mjs", ".cjs"];

#[derive(Default)]
pub struct LoadedResources {
    pub registry: ResourceRegistry,
    pub context_files: Vec<String>,
    pub system_prompt: String,
}

enum ManifestValue {
    Paths(Vec<PathBuf>),
    Conventional,
}

#[derive(Default)]
struct Manifest {
    pi: [(bool, Vec<String>); 4],
}

fn manifest_paths(root: &Path, patterns: &[String]) -> Vec<PathBuf> {
    // Port of `manifestPaths`: `!` exclusions and `*`/`**`/`?` glob matching.
    let mut included: Vec<PathBuf> = Vec::new();
    let mut excluded: Vec<String> = Vec::new();
    for raw in patterns {
        let pattern = raw.trim_start_matches('+');
        if let Some(negated) = pattern.strip_prefix('!') {
            excluded.push(negated.trim_start_matches('+').to_string());
            continue;
        }
        push_pattern(root, pattern, &mut included);
    }
    let unique: Vec<PathBuf> = {
        let mut seen = std::collections::HashSet::new();
        included.retain(|path| seen.insert(path.to_string_lossy().to_string()));
        included
    };
    if excluded.is_empty() {
        return unique;
    }
    unique
        .into_iter()
        .filter(|path| {
            let relative = path
                .strip_prefix(root)
                .map(|relative| relative.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| path.to_string_lossy().to_string());
            !excluded.iter().any(|pattern| matches_glob(&relative, pattern))
        })
        .collect()
}

fn push_pattern(root: &Path, pattern: &str, out: &mut Vec<PathBuf>) {
    if pattern.contains('*') || pattern.contains('?') {
        let base = root.to_path_buf();
        for file in walk_files(&base) {
            let relative = file
                .strip_prefix(root)
                .map(|relative| relative.to_string_lossy().replace('\\', "/"))
                .unwrap_or_else(|_| file.to_string_lossy().to_string());
            if matches_glob(&relative, pattern) {
                out.push(file);
            }
        }
    } else {
        let path = root.join(pattern);
        if path.is_dir() {
            out.extend(walk_files(&path));
        } else if path.exists() {
            out.push(path);
        }
    }
}

pub fn walk_files(root: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    if root.is_file() {
        files.push(root.to_path_buf());
        return files;
    }
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&directory) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if matches!(name.as_str(), ".git" | "node_modules" | "dist") {
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

/// Port of `globRegex`: `**` → any run, `*` → no separator, `?` → one char.
fn matches_glob(path: &str, pattern: &str) -> bool {
    // Convert to a bounded substring matcher by splitting on the separator.
    let pattern_segments: Vec<&str> = pattern.split('/').collect();
    let path_segments: Vec<&str> = path.split('/').collect();
    segment_match(&pattern_segments, &path_segments)
}

fn segment_match<'a>(pattern: &[&'a str], path: &[&str]) -> bool {
    if pattern.is_empty() {
        return path.is_empty();
    }
    let Some(head) = pattern.first() else { return false };
    if *head == "**" {
        // Consume any number of path segments.
        return (0..=path.len()).any(|skip| segment_match(&pattern[1..], &path[skip..]));
    }
    if path.is_empty() {
        return false;
    }
    if segment_wildcard_match(head, path[0]) {
        segment_match(&pattern[1..], &path[1..])
    } else {
        false
    }
}

fn segment_wildcard_match(pattern: &str, value: &str) -> bool {
    // `*` → any run within the segment; `?` → any single char.
    let pattern_chars: Vec<char> = pattern.chars().collect();
    let value_chars: Vec<char> = value.chars().collect();
    wildcard(&pattern_chars, &value_chars)
}

fn wildcard(pattern: &[char], value: &[char]) -> bool {
    if pattern.is_empty() {
        return value.is_empty();
    }
    match pattern[0] {
        '*' => (0..=value.len()).any(|skip| wildcard(&pattern[1..], &value[skip..])),
        '?' => !value.is_empty() && wildcard(&pattern[1..], &value[1..]),
        ch => !value.is_empty() && value[0] == ch && wildcard(&pattern[1..], &value[1..]),
    }
}

fn frontmatter(content: &str) -> BTreeMap<String, String> {
    let trimmed = content.trim_start_matches(['\n', '\r']);
    if !trimmed.starts_with("---") {
        return BTreeMap::new();
    }
    let rest = &trimmed[3..];
    let Some(end) = rest.find("\n---") else { return BTreeMap::new() };
    let body = &rest[..end];
    let mut values = BTreeMap::new();
    for line in body.lines() {
        if let Some(separator) = line.find(':') {
            let key = line[..separator].trim().to_string();
            let mut value = line[separator + 1..].trim().to_string();
            value = value.trim_matches(['\'', '"']).to_string();
            values.insert(key, value);
        }
    }
    values
}

fn first_line(content: &str) -> &str {
    content.split('\n').map(str::trim).find(|line| !line.is_empty()).unwrap_or("")
}

fn register_file(registry: &mut ResourceRegistry, kind: ResourceKind, file: &Path) {
    let name = file.file_stem().and_then(|stem| stem.to_str()).unwrap_or_default().to_string();
    let extension = file.extension().and_then(|ext| ext.to_str()).unwrap_or_default();
    let dot_extension = format!(".{extension}");
    match kind {
        ResourceKind::Extension => {
            if !EXTENSION_EXTS.contains(&dot_extension.as_str()) {
                return;
            }
        }
        ResourceKind::Prompt => {
            if dot_extension != ".md" {
                return;
            }
        }
        ResourceKind::Theme => {
            if dot_extension != ".json" {
                return;
            }
        }
        ResourceKind::Skill => {}
    }
    let mut description: Option<String> = None;
    if let Ok(content) = std::fs::read_to_string(file) {
        match kind {
            ResourceKind::Skill => description = frontmatter(&content).get("description").cloned(),
            ResourceKind::Prompt => description = Some(frontmatter(&content).get("description").cloned().unwrap_or_else(|| first_line(&content).to_string())),
            ResourceKind::Theme => {
                description = serde_json::from_str::<Value>(&content).ok().and_then(|value| value["name"].as_str().map(str::to_string));
            }
            ResourceKind::Extension => {}
        }
    }
    let display_name = if kind == ResourceKind::Theme && description.as_ref().is_some_and(|name| !name.is_empty()) {
        description.clone().unwrap()
    } else {
        name.clone()
    };
    let _ = registry.register(ResourceEntry { kind, name: display_name, path: file.to_string_lossy().to_string(), description });
}

fn register_skills(registry: &mut ResourceRegistry, root: &Path) {
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        let skill_file = directory.join("SKILL.md");
        if let Ok(content) = std::fs::read_to_string(&skill_file) {
            let metadata = frontmatter(&content);
            if let (Some(name), Some(description)) = (metadata.get("name"), metadata.get("description")) {
                let _ = registry.register(ResourceEntry {
                    kind: ResourceKind::Skill,
                    name: name.clone(),
                    path: directory.to_string_lossy().to_string(),
                    description: Some(description.clone()),
                });
            }
        }
        if let Ok(entries) = std::fs::read_dir(&directory) {
            for entry in entries.flatten() {
                let name = entry.file_name().to_string_lossy().to_string();
                if matches!(name.as_str(), "node_modules" | ".git") {
                    continue;
                }
                if entry.path().is_dir() {
                    stack.push(entry.path());
                }
            }
        }
    }
}

fn read_manifest(root: &Path) -> Result<Option<Manifest>> {
    let file = root.join("package.json");
    if !file.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&file).map_err(|error| anyhow!("Invalid package manifest {}: {error}", file.display()))?;
    let value: Value = serde_json::from_str(&content).map_err(|error| anyhow!("Invalid package manifest {}: {error}", file.display()))?;
    let mut manifest = Manifest::default();
    for (index, (kind, _)) in CONVENTIONAL.iter().enumerate() {
        if let Some(patterns) = value["pi"][kind.as_str()].as_array() {
            manifest.pi[index] = (true, patterns.iter().filter_map(Value::as_str).map(str::to_string).collect());
        } else if let Some(pattern) = value["pi"][kind.as_str()].as_str() {
            manifest.pi[index] = (true, vec![pattern.to_string()]);
        }
    }
    Ok(Some(manifest))
}

fn discover(root: &Path, registry: &mut ResourceRegistry) -> Result<()> {
    let manifest = read_manifest(root)?;
    for (index, (kind, conventional)) in CONVENTIONAL.iter().enumerate() {
        let manifest_value = manifest.as_ref().and_then(|manifest| {
            let (has, patterns) = (&manifest.pi[index].0, &manifest.pi[index].1);
            #[allow(clippy::collapsible_if)]
            if std::env::var("HARNESS_DEBUG_LOAD").as_deref() == Ok("1") {
                eprintln!("[discover] kind={kind:?} has={has} patterns={patterns:?}");
            }
            has.then(|| patterns.clone())
        });
        match manifest_value {
            Some(patterns) => {
                let paths = manifest_paths(root, &patterns);
                if *kind == ResourceKind::Skill {
                    let mut directories: Vec<PathBuf> = Vec::new();
                    for path in paths {
                        let directory = if path.is_dir() { path } else { path.parent().unwrap_or(Path::new(".")).to_path_buf() };
                        if !directories.contains(&directory) {
                            directories.push(directory);
                        }
                    }
                    for directory in directories {
                        register_skills(registry, &directory);
                    }
                } else {
                    for path in paths {
                        register_file(registry, *kind, &path);
                    }
                }
            }
            None => {
                let directory = root.join(conventional);
                if !directory.is_dir() {
                    continue;
                }
                if *kind == ResourceKind::Skill {
                    register_skills(registry, &directory);
                    continue;
                }
                let entries: Vec<std::fs::DirEntry> = std::fs::read_dir(&directory)?.flatten().collect();
                for entry in entries {
                    let path = entry.path();
                    if path.is_file() {
                        register_file(registry, *kind, &path);
                    } else if *kind == ResourceKind::Extension && path.is_dir() {
                        for candidate in ["index.ts", "index.js", "index.mjs", "index.cjs"] {
                            let index = path.join(candidate);
                            if index.exists() {
                                register_file(registry, *kind, &index);
                            }
                        }
                        // Non-index entry files inside nested extension directories count too.
                        if let Ok(nested_entries) = std::fs::read_dir(&path) {
                            for nested in nested_entries.flatten() {
                                let nested_name = nested.file_name().to_string_lossy().to_string();
                                if nested_name.ends_with(".ts") || nested_name.ends_with(".js") || nested_name.ends_with(".mjs") || nested_name.ends_with(".cjs") {
                                    register_file(registry, *kind, &nested.path());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    Ok(())
}

pub fn context_candidates(directory: &Path) -> Vec<PathBuf> {
    ["AGENTS.override.md", "AGENTS.md", "AGENTS.MD", "CLAUDE.md", "CLAUDE.MD"]
        .iter()
        .map(|name| directory.join(name))
        .filter(|path| path.exists())
        .collect()
}

fn load_context_files(cwd: &Path, personal_dir: &Path) -> Vec<String> {
    let mut files: Vec<String> = Vec::new();
    let mut directories = vec![personal_dir.to_path_buf()];
    let mut current = cwd.to_path_buf();
    loop {
        directories.push(current.clone());
        let Some(parent) = current.parent() else { break };
        if parent == current {
            break;
        }
        current = parent.to_path_buf();
    }
    for directory in directories {
        let candidates = context_candidates(&directory);
        let selected = candidates.iter().find(|path| path.ends_with("AGENTS.override.md")).or_else(|| candidates.iter().find(|path| !path.ends_with("AGENTS.override.md")));
        if let Some(selected) = selected {
            let value = selected.to_string_lossy().to_string();
            if !files.contains(&value) {
                files.push(value);
            }
        }
    }
    files
}

/// Expand prompt-template placeholders, mirroring the TS `expandPromptTemplate`.
pub fn expand_prompt_template(path: &Path, args: &str) -> Result<String> {
    let content = std::fs::read_to_string(path)?;
    let trimmed = strip_frontmatter(&content).trim().to_string();
    let words: Vec<&str> = args.split_whitespace().collect();
    let mut output = trimmed
        .replace("$ARGUMENTS", args)
        .replace("${ARGUMENTS}", args)
        .replace("$@", args)
        .replace("$$", "");
    // ${0}/$1.. positional substitutions
    let mut result = String::new();
    let mut chars = output.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '$' {
            if chars.peek() == Some(&'{') {
                let mut bracketed = String::new();
                chars.next();
                for inner in chars.by_ref() {
                    if inner == '}' {
                        break;
                    }
                    bracketed.push(inner);
                }
                if let Ok(index) = bracketed.parse::<usize>() {
                    result.push_str(words.get(index.saturating_sub(1)).copied().unwrap_or(""));
                } else if bracketed == "ARGUMENTS" {
                    result.push_str(args);
                }
                continue;
            }
            if ch == '$' && chars.peek() == Some(&'$') {
                chars.next();
                result.push_str("");
                continue;
            }
            let mut digits = String::new();
            for next in chars.peek().copied() {
                if next.is_ascii_digit() {
                    digits.push(next);
                    chars.next();
                } else {
                    break;
                }
            }
            if !digits.is_empty() {
                if let Ok(index) = digits.parse::<usize>() {
                    result.push_str(words.get(index.saturating_sub(1)).copied().unwrap_or(""));
                    continue;
                }
            }
        }
        result.push(ch);
    }
    Ok(result)
}

fn strip_frontmatter(content: &str) -> &str {
    let trimmed = content.trim_start_matches(['\n', '\r']);
    if trimmed.starts_with("---") {
        if let Some((_, rest)) = trimmed.split_once("\n---") {
            let after = rest.trim_start_matches("---");
            return after.trim_start_matches(['\n']);
        }
    }
    trimmed
}

pub fn load_skill_instructions(path: &Path) -> Result<String> {
    let content = std::fs::read_to_string(path.join("SKILL.md"))?;
    Ok(strip_frontmatter(&content).trim().to_string())
}

pub struct ResourceLoadOptions {
    pub cwd: PathBuf,
    pub personal_dir: PathBuf,
    pub include_project: bool,
    pub project_trusted: bool,
    pub package_roots: Vec<PathBuf>,
}

pub fn load_resources(options: &ResourceLoadOptions) -> Result<LoadedResources> {
    let mut registry = ResourceRegistry::default();
    let mut roots: Vec<PathBuf> = vec![options.personal_dir.clone()];
    if options.include_project && options.project_trusted {
        roots.push(options.cwd.clone());
        roots.push(options.cwd.join(".pi"));
    }
    roots.extend(options.package_roots.iter().cloned());
    for root in roots {
        if root.exists() {
            discover(&root, &mut registry)?;
        }
    }
    let context_files = load_context_files(&options.cwd, &options.personal_dir);
    let mut context = String::new();
    if !context_files.is_empty() {
        context.push_str("\n\nContext files:");
        for file in &context_files {
            context.push_str(&format!("\n\n--- {file} ---\n{}", std::fs::read_to_string(file)?));
        }
    }
    let project_scope = |file: PathBuf| options.project_trusted || !file.starts_with(options.cwd.join(".pi"));
    let system_file = [options.cwd.join(".pi").join("SYSTEM.md"), options.personal_dir.join("SYSTEM.md")]
        .into_iter()
        .find(|file| file.exists() && project_scope(file.clone()));
    let append_file = [options.cwd.join(".pi").join("APPEND_SYSTEM.md"), options.personal_dir.join("APPEND_SYSTEM.md")]
        .into_iter()
        .find(|file| file.exists() && project_scope(file.clone()));
    let mut system = String::new();
    if let Some(system_file) = system_file {
        system.push_str(&format!("\n\nSystem instructions:\n{}", std::fs::read_to_string(system_file)?));
    }
    if let Some(append_file) = append_file {
        system.push_str(&format!("\n\nAdditional instructions:\n{}", std::fs::read_to_string(append_file)?));
    }
    let skills = registry.list(Some(ResourceKind::Skill));
    if !skills.is_empty() {
        system.push_str("\n\nAvailable skills:");
        for skill in skills {
            system.push_str(&format!("\n- {}: {} ({})", skill.name, skill.description.clone().unwrap_or_else(|| "read SKILL.md".into()), skill.path));
        }
    }
    Ok(LoadedResources { registry, context_files, system_prompt: format!("You are a helpful coding agent.{system}{context}") })
}

pub struct DebugManifest {
    pub pi: [(bool, Vec<String>); 4],
}

pub fn probe_manifest(root: &Path) -> Option<DebugManifest> {
    read_manifest(root).ok().flatten().map(|manifest| DebugManifest { pi: manifest.pi })
}

pub fn manifest_paths_probe(root: &Path, patterns: &[String]) -> Vec<PathBuf> {
    manifest_paths(root, patterns)
}

#[doc(hidden)]
pub fn debug_glob_match(path: &str, pattern: &str) -> bool {
    matches_glob(path, pattern)
}

#[doc(hidden)]
pub fn walk_files_probe(root: &Path) -> Vec<PathBuf> {
    walk_files(root)
}

#[doc(hidden)]
pub fn debug_manifest_paths(root: &Path, patterns: &[String]) -> (Vec<PathBuf>, Vec<String>) {
    let mut included = Vec::new();
    let mut excluded = Vec::new();
    for raw in patterns {
        let pattern = raw.trim_start_matches('+');
        if let Some(negated) = pattern.strip_prefix('!') {
            excluded.push(negated.trim_start_matches('+').to_string());
            continue;
        }
        let mut one = Vec::new();
        push_pattern(root, pattern, &mut one);
        eprintln!("[glob] {pattern:?} -> {}", one.len());
        included.extend(one);
    }
    (included, excluded)
}

#[doc(hidden)]
pub fn debug_discover(root: &Path) -> (usize, usize, usize, usize) {
    let mut registry = super::registry::ResourceRegistry::default();
    let _ = discover(root, &mut registry);
    (
        registry.list(Some(ResourceKind::Extension)).len(),
        registry.list(Some(ResourceKind::Skill)).len(),
        registry.list(Some(ResourceKind::Prompt)).len(),
        registry.list(Some(ResourceKind::Theme)).len(),
    )
}
