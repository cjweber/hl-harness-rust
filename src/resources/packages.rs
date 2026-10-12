//! Package management, ported from `src/resources/packages.ts`: npm/git
//! resolution (including Pi's mirrored git layout), settings-file management,
//! and tolerant roots resolution.

use crate::config::settings_layers;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ManagedPackage {
    pub source: String,
    #[serde(default)]
    pub enabled: bool,
}

pub struct PackageResolution {
    pub source: String,
    pub root: PathBuf,
    pub installed: bool,
}

fn npm_root(personal_dir: &Path, name: &str) -> PathBuf {
    let direct = personal_dir.join("npm").join(name);
    if direct.exists() {
        return direct;
    }
    personal_dir.join("npm").join("node_modules").join(name)
}

fn git_root(personal_dir: &Path, source: &str) -> PathBuf {
    // Pi installs git packages under a mirrored URL path; reuse it when present
    // so an already-installed package is reused instead of cloned twice.
    let mirrored = personal_dir.join("git").join(source.split('/').collect::<PathBuf>());
    if mirrored.exists() {
        return mirrored;
    }
    let mut hasher = Sha256::new();
    hasher.update(source.as_bytes());
    let digest = hasher.finalize()[..8].iter().map(|byte| format!("{byte:02x}")).collect::<String>();
    personal_dir.join("git").join(digest)
}

fn clone(source: &str, target: &Path) -> Result<()> {
    let status = std::process::Command::new("git").arg("clone").arg("--depth").arg("1").arg(source).arg(target).status()?;
    if !status.success() {
        bail!("git clone failed for {source}");
    }
    Ok(())
}

pub struct ResolveOptions<'a> {
    pub cwd: &'a Path,
    pub personal_dir: &'a Path,
    pub install: bool,
}

pub fn resolve_package(source: &str, options: &ResolveOptions<'_>) -> Result<PackageResolution> {
    if let Some(spec) = source.strip_prefix("npm:") {
        let name = if spec.starts_with('@') {
            spec.split('/').take(2).collect::<Vec<_>>().join("/")
        } else {
            spec.split('@').next().unwrap_or(spec).to_string()
        };
        let root = npm_root(options.personal_dir, &name);
        if !root.exists() && options.install {
            let npm_dir = options.personal_dir.join("npm");
            let status = std::process::Command::new("npm").arg("install").arg("--prefix").arg(&npm_dir).arg(spec).status()?;
            if !status.success() {
                bail!("npm install failed for {spec}");
            }
        }
        if !root.exists() {
            bail!("Package is not installed: {source} (expected {})", root.display());
        }
        return Ok(PackageResolution { source: source.to_string(), root, installed: true });
    }
    if source.starts_with("git:") || source.starts_with("https://") || source.starts_with("git@") {
        let git_source = source.strip_prefix("git:").unwrap_or(source);
        let root = git_root(options.personal_dir, git_source);
        if !root.exists() && options.install {
            clone(git_source, &root)?;
        }
        if !root.exists() {
            bail!("Package is not installed: {source} (expected {})", root.display());
        }
        return Ok(PackageResolution { source: source.to_string(), root, installed: true });
    }
    let root = options.cwd.join(source);
    if !root.exists() {
        bail!("Local package does not exist: {}", root.display());
    }
    if root.to_string_lossy().ends_with(".ts") && root.to_string_lossy().ends_with(".js") {
        let parent = root.parent().unwrap_or(Path::new(".")).to_path_buf();
        return Ok(PackageResolution { source: source.to_string(), root: parent, installed: true });
    }
    if root.is_file() && (root.extension().and_then(|ext| ext.to_str()).unwrap_or_default() == "ts" || root.extension().and_then(|ext| ext.to_str()).unwrap_or_default() == "js") {
        let parent = root.parent().unwrap_or(Path::new(".")).to_path_buf();
        return Ok(PackageResolution { source: source.to_string(), root: parent, installed: true });
    }
    Ok(PackageResolution { source: source.to_string(), root, installed: true })
}

pub fn install_package(source: &str, options: &ResolveOptions<'_>) -> Result<PackageResolution> {
    resolve_package(source, &ResolveOptions { install: true, ..ResolveOptions { cwd: options.cwd, personal_dir: options.personal_dir, install: false } })
}

pub fn update_package(source: &str, options: &ResolveOptions<'_>) -> Result<PackageResolution> {
    if let Some(spec) = source.strip_prefix("npm:") {
        let name = if spec.starts_with('@') {
            spec.split('/').take(2).collect::<Vec<_>>().join("/")
        } else {
            spec.split('@').next().unwrap_or(spec).to_string()
        };
        let status = std::process::Command::new("npm").arg("update").arg("--prefix").arg(options.personal_dir.join("npm")).arg(&name).status()?;
        if !status.success() {
            bail!("npm update failed for {spec}");
        }
        return resolve_package(source, options);
    }
    if source.starts_with("git:") || source.starts_with("https://") || source.starts_with("git@") {
        let resolution = resolve_package(source, options)?;
        let status = std::process::Command::new("git").arg("-C").arg(&resolution.root).arg("pull").arg("--ff-only").status()?;
        if !status.success() {
            bail!("git pull failed for {source}");
        }
        return Ok(PackageResolution { source: source.to_string(), root: resolution.root, installed: true });
    }
    resolve_package(source, options)
}

/// Resolve package sources, skipping ones that are not installed (like the TS
/// `resolvePackageRoots`, so a missing package warns instead of aborting).
pub fn resolve_package_roots(sources: &[String], options: &ResolveOptions<'_>, warn: &dyn Fn(&str)) -> Vec<PathBuf> {
    let mut roots = Vec::new();
    for source in sources {
        match resolve_package(source, options) {
            Ok(resolution) => roots.push(resolution.root),
            Err(error) => warn(&format!("skipping {source}: {error}")),
        }
    }
    roots
}

/// Settings layer files that declare packages, highest priority first.
fn package_files(personal_dir: &Path, project_dir: Option<&Path>) -> Vec<PathBuf> {
    settings_layers(project_dir.unwrap_or(Path::new(".")), personal_dir)
}

pub fn read_managed_packages(file: &Path) -> Vec<ManagedPackage> {
    let Ok(content) = std::fs::read_to_string(file) else { return Vec::new() };
    let Ok(value) = serde_json::from_str::<Value>(&content) else { return Vec::new() };
    let Some(packages) = value.get("packages").and_then(Value::as_array) else { return Vec::new() };
    packages
        .iter()
        .filter_map(|item| {
            if let Some(source) = item.as_str() {
                return Some(ManagedPackage { source: source.to_string(), enabled: true });
            }
            let source = item.get("source")?.as_str()?.to_string();
            Some(ManagedPackage { source, enabled: item.get("enabled").and_then(Value::as_bool).unwrap_or(true) })
        })
        .collect()
}

fn write_managed_packages(file: &Path, packages: &[ManagedPackage]) -> Result<()> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let value = json!({"packages": packages});
    std::fs::write(file, serde_json::to_string_pretty(&value)? + "\n")?;
    Ok(())
}

#[derive(Clone, Debug)]
pub struct ManagedPackageRef {
    pub file: PathBuf,
    pub package: ManagedPackage,
}

pub fn list_managed_packages(personal_dir: &Path, project_dir: Option<&Path>) -> Vec<ManagedPackageRef> {
    package_files(personal_dir, project_dir)
        .into_iter()
        .flat_map(|file| read_managed_packages(&file).into_iter().map(move |package| ManagedPackageRef { file: file.clone(), package }))
        .collect()
}

pub fn set_package_enabled(file: &Path, source: &str, enabled: bool) -> Result<()> {
    let mut packages = read_managed_packages(file);
    let Some(existing) = packages.iter_mut().find(|candidate| candidate.source == source) else {
        bail!("Package is not configured: {source}");
    };
    existing.enabled = enabled;
    write_managed_packages(file, &packages)
}

pub fn remove_managed_package(file: &Path, source: &str) -> Result<bool> {
    let packages = read_managed_packages(file);
    let next: Vec<ManagedPackage> = packages.iter().filter(|candidate| candidate.source != source).cloned().collect();
    if next.len() == packages.len() {
        return Ok(false);
    }
    write_managed_packages(file, &next)?;
    Ok(true)
}

pub fn package_sources_from_settings(personal_dir: &Path, project_dir: Option<&Path>) -> Vec<String> {
    let mut sources: BTreeMap<String, ()> = BTreeMap::new();
    for file in package_files(personal_dir, project_dir) {
        let base = file.parent().map(Path::parent).unwrap_or(Some(Path::new(".")));
        for item in read_managed_packages(&file) {
            if !item.enabled {
                continue;
            }
            if item.source.starts_with("npm:") || item.source.starts_with("git:") || item.source.starts_with("https://") || item.source.starts_with("git@") {
                sources.insert(item.source, ());
            } else {
                let absolute = base.unwrap_or(Path::new(".")).join(&item.source);
                sources.insert(absolute.to_string_lossy().to_string(), ());
            }
        }
    }
    sources.into_keys().collect()
}
