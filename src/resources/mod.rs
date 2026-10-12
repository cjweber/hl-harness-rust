pub mod loader;
pub mod packages;
pub mod registry;
pub use loader::{expand_prompt_template, load_resources, load_skill_instructions, ResourceLoadOptions};
pub use registry::{ResourceEntry, ResourceKind, ResourceRegistry};
