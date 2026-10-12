
use hl_harness::resources::{load_resources, ResourceEntry, ResourceKind, ResourceLoadOptions, ResourceRegistry};
use serde_json::json;
use std::path::Path;

fn temp_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("harness-resources-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn options(cwd: &Path, personal: &Path, trusted: bool, packages: Vec<std::path::PathBuf>) -> ResourceLoadOptions {
    ResourceLoadOptions {
        cwd: cwd.to_path_buf(),
        personal_dir: personal.to_path_buf(),
        include_project: true,
        project_trusted: trusted,
        package_roots: packages,
    }
}

#[test]
fn registry_is_sorted_and_rejects_duplicates() {
    let mut registry = ResourceRegistry::default();
    registry.register(ResourceEntry { kind: ResourceKind::Skill, name: "zebra".into(), path: "/z".into(), description: None }).unwrap();
    registry.register(ResourceEntry { kind: ResourceKind::Prompt, name: "alpha".into(), path: "/a".into(), description: None }).unwrap();
    registry.register(ResourceEntry { kind: ResourceKind::Skill, name: "alpha".into(), path: "/b".into(), description: None }).unwrap();
    let all = registry.list(None);
    assert_eq!(all[0].name, "alpha");
    assert_eq!(registry.list(Some(ResourceKind::Skill)).len(), 2);
    let duplicate = registry.register(ResourceEntry { kind: ResourceKind::Skill, name: "zebra".into(), path: "/other".into(), description: None });
    assert!(duplicate.is_err());
}

#[test]
fn discover_conventional_directories_and_skill_frontmatter() {
    let root = temp_dir();
    std::fs::create_dir_all(root.join("skills/nested")).unwrap();
    std::fs::create_dir_all(root.join("prompts")).unwrap();
    std::fs::create_dir_all(root.join("themes")).unwrap();
    std::fs::write(root.join("skills/nested/SKILL.md"), "---\nname: deploy\ndescription: Ship it\n---\nBODY").unwrap();
    std::fs::write(root.join("prompts/plan.md"), "Do the thing").unwrap();
    std::fs::write(root.join("themes/pastel.json"), json!({"name": "pastel"}).to_string()).unwrap();
    std::fs::create_dir_all(root.join("extensions/widget")).unwrap();
    std::fs::write(root.join("extensions/index.js"), "export default () => ({})").unwrap();
    std::fs::write(root.join("extensions/widget/tool.ts"), "export default () => ({})").unwrap();

    let resources = load_resources(&options(&root, &root, true, Vec::new())).unwrap();
    let names: Vec<String> = resources.registry.list(None).iter().map(|entry| entry.name.clone()).collect();
    assert!(names.contains(&"deploy".to_string()), "{names:?}");
    assert!(names.contains(&"plan".to_string()));
    assert!(names.contains(&"pastel".to_string()));
    assert_eq!(resources.registry.list(Some(ResourceKind::Extension)).len(), 2);
}

#[test]
fn manifests_expand_globs_and_exclusions() {
    let root = temp_dir();
    std::fs::create_dir_all(root.join("feature/deep/nested")).unwrap();
    std::fs::write(root.join("package.json"), json!({"name": "p", "pi": {"skill": ["feature/**/SKILL.md", "!feature/hidden/SKILL.md"]}}).to_string()).unwrap();
    std::fs::create_dir_all(root.join("feature/kept")).unwrap();
    std::fs::create_dir_all(root.join("feature/hidden")).unwrap();
    std::fs::write(root.join("feature/kept/SKILL.md"), "---\nname: kept\ndescription: yes\n---").unwrap();
    std::fs::write(root.join("feature/hidden/SKILL.md"), "---\nname: hidden\ndescription: no\n---").unwrap();
    std::fs::write(root.join("feature/deep/nested/SKILL.md"), "---\nname: nested\ndescription: deep\n---").unwrap();
    let resources = load_resources(&options(&root, &root, true, Vec::new())).unwrap();
    let names: Vec<String> = resources.registry.list(Some(ResourceKind::Skill)).iter().map(|entry| entry.name.clone()).collect();
    assert!(names.contains(&"kept".to_string()) && names.contains(&"nested".to_string()), "{names:?}");
    assert!(!names.contains(&"hidden".to_string()), "{names:?}");
}

#[test]
fn context_files_walk_and_system_prompt() {
    let root = temp_dir();
    let personal = temp_dir();
    let workdir = root.join("project");
    std::fs::create_dir_all(&workdir).unwrap();
    std::fs::write(workdir.join("AGENTS.md"), "Project context body").unwrap();
    std::fs::write(workdir.join("AGENTS.override.md"), "Override wins").unwrap();
    std::fs::write(personal.join("SYSTEM.md"), "Personal system rules").unwrap();
    let resources = load_resources(&options(&workdir, &personal, true, Vec::new())).unwrap();
    assert_eq!(resources.context_files.len(), 1);
    assert!(resources.context_files[0].ends_with("AGENTS.override.md"));
    assert!(resources.system_prompt.contains("Personal system rules"));
    assert!(resources.system_prompt.contains("Override wins"));
}

#[test]
fn prompt_templates_expand_arguments() {
    let root = temp_dir();
    let prompt = root.join("plan.md");
    std::fs::write(&prompt, "Args are $ARGUMENTS, first=$1, second=$2, third=${3}, none=$4\n").unwrap();
    let expanded = hl_harness::resources::expand_prompt_template(&prompt, "alpha beta gamma").unwrap();
    assert!(expanded.contains("Args are alpha beta gamma, first=alpha, second=beta, third=gamma, none="), "{expanded}");
}

#[test]
fn packages_resolve_npm_git_and_settings_round_trip() {
    let personal = temp_dir();
    let project = temp_dir();
    // npm layout
    let npm_name = "some-package";
    std::fs::create_dir_all(personal.join("npm").join("node_modules").join(npm_name)).unwrap();
    let resolved = hl_harness::resources::packages::resolve_package(
        "npm:some-package",
        &hl_harness::resources::packages::ResolveOptions { cwd: &project, personal_dir: &personal, install: false },
    ).unwrap();
    assert!(resolved.root.ends_with("node_modules/some-package"), "{}", resolved.root.display());
    let missing = hl_harness::resources::packages::resolve_package(
        "npm:not-there",
        &hl_harness::resources::packages::ResolveOptions { cwd: &project, personal_dir: &personal, install: false },
    );
    assert!(missing.is_err());
    // Pi git mirrored layout
    let git_root = personal.join("git/github.com/org/repo");
    std::fs::create_dir_all(&git_root).unwrap();
    let git_resolved = hl_harness::resources::packages::resolve_package(
        "git:github.com/org/repo",
        &hl_harness::resources::packages::ResolveOptions { cwd: &project, personal_dir: &personal, install: false },
    ).unwrap();
    assert_eq!(git_resolved.root, git_root);
    // settings round trip: enable/disable/remove
    let settings = personal.join("settings.json");
    std::fs::write(&settings, serde_json::to_string(&json!({"packages": ["npm:some-package", {"source": "git:github.com/org/repo", "enabled": false}]})).unwrap()).unwrap();
    let sources = hl_harness::resources::packages::package_sources_from_settings(&personal, Some(&project));
    assert_eq!(sources, vec!["npm:some-package".to_string()]);
    hl_harness::resources::packages::set_package_enabled(&settings, "npm:some-package", false).unwrap();
    assert!(hl_harness::resources::packages::package_sources_from_settings(&personal, Some(&project)).is_empty());
    // warnings instead of fatal errors
    let warns: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
    let warns_clone = warns.clone();
    let roots = hl_harness::resources::packages::resolve_package_roots(
        &["npm:missing".to_string()],
        &hl_harness::resources::packages::ResolveOptions { cwd: &project, personal_dir: &personal, install: false },
        &move |message: &str| warns_clone.lock().unwrap().push(message.to_string()),
    );
    assert!(roots.is_empty());
    assert_eq!(warns.lock().unwrap().len(), 1);
}
