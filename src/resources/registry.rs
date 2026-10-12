//! Resource registry, ported from `src/resources/registry.ts`.

use anyhow::{anyhow, bail, Result};
use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum ResourceKind {
    Extension,
    Skill,
    Prompt,
    Theme,
}

impl ResourceKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Extension => "extension",
            Self::Skill => "skill",
            Self::Prompt => "prompt",
            Self::Theme => "theme",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct ResourceEntry {
    pub kind: ResourceKind,
    pub name: String,
    pub path: String,
    pub description: Option<String>,
}

#[derive(Default)]
pub struct ResourceRegistry {
    resources: BTreeMap<String, ResourceEntry>,
}

impl ResourceRegistry {
    pub fn register(&mut self, resource: ResourceEntry) -> Result<()> {
        let key = format!("{}:{}", resource.kind.as_str(), resource.name);
        if let Some(existing) = self.resources.get(&key) {
            bail!("Duplicate resource {key} at {} and {}", existing.path, resource.path);
        }
        self.resources.insert(key, resource);
        Ok(())
    }

    pub fn list(&self, kind: Option<ResourceKind>) -> Vec<ResourceEntry> {
        self.resources
            .values()
            .filter(|entry| kind.is_none_or(|target| entry.kind == target))
            .cloned()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.resources.len()
    }

    pub fn is_empty(&self) -> bool {
        self.resources.is_empty()
    }
}

#[allow(dead_code)]
fn placeholder(value: Option<ResourceKind>) -> Option<BTreeMap<String, usize>> {
    value.map(|kind| BTreeMap::from([(kind.as_str().to_string(), 0)]))
}

#[allow(dead_code)]
fn unused() -> std::result::Result<(), anyhow::Error> {
    Err(anyhow!("unused"))
}
