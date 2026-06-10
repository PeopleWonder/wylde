//! The embedded system-prompt catalog — groups, labels, and default texts.
//!
//! Rust port of `Core/shared/system_prompts_catalog.py` (full-Rust
//! cutover, 2026-06-09). The catalog data lives in `catalog.json`,
//! generated VERBATIM from the Python module at port time and embedded
//! via `include_str!` (the `visual_style_v1.yaml` pattern): defaults,
//! groupings, and labels have exactly one home. The Settings page reads
//! all of it through `prompts.list` so a clean install with no override
//! file behaves identically to one carrying catalog defaults.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;
use serde_json::{json, Value};

/// A Settings-page section of related prompts.
#[derive(Debug, Clone, Deserialize)]
pub struct PromptGroup {
    pub id: String,
    pub label: String,
    pub blurb: String,
}

/// One overridable prompt: identity, grouping, and the catalog default.
#[derive(Debug, Clone, Deserialize)]
pub struct PromptEntry {
    pub id: String,
    pub group: String,
    pub label: String,
    pub desc: String,
    pub default: String,
}

#[derive(Debug, Deserialize)]
struct CatalogFile {
    groups: Vec<PromptGroup>,
    catalog: Vec<PromptEntry>,
}

struct Catalog {
    file: CatalogFile,
    by_id: HashMap<String, usize>,
}

static CATALOG: OnceLock<Catalog> = OnceLock::new();

fn catalog() -> &'static Catalog {
    CATALOG.get_or_init(|| {
        let file: CatalogFile = serde_json::from_str(include_str!("catalog.json"))
            .expect("embedded prompt catalog parses");
        let by_id = file
            .catalog
            .iter()
            .enumerate()
            .map(|(i, e)| (e.id.clone(), i))
            .collect();
        Catalog { file, by_id }
    })
}

/// The catalog entry for `prompt_id`, if it exists.
pub fn entry_for(prompt_id: &str) -> Option<&'static PromptEntry> {
    let c = catalog();
    c.by_id.get(prompt_id).map(|&i| &c.file.catalog[i])
}

/// The catalog default text for `prompt_id` (empty string for unknown ids,
/// mirroring the Python `default_for`).
pub fn default_for(prompt_id: &str) -> &'static str {
    entry_for(prompt_id)
        .map(|e| e.default.as_str())
        .unwrap_or("")
}

/// Every catalog id, in catalog order.
pub fn all_ids() -> Vec<&'static str> {
    catalog()
        .file
        .catalog
        .iter()
        .map(|e| e.id.as_str())
        .collect()
}

/// The groups as the wire-shape JSON array (`[{id,label,blurb}]`).
pub fn groups_json() -> Value {
    Value::Array(
        catalog()
            .file
            .groups
            .iter()
            .map(|g| json!({ "id": g.id, "label": g.label, "blurb": g.blurb }))
            .collect(),
    )
}

/// The catalog as the wire-shape JSON array
/// (`[{id,group,label,desc,default}]`).
pub fn catalog_json() -> Value {
    Value::Array(
        catalog()
            .file
            .catalog
            .iter()
            .map(|e| {
                json!({
                    "id": e.id,
                    "group": e.group,
                    "label": e.label,
                    "desc": e.desc,
                    "default": e.default,
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_catalog_parses_and_is_nonempty() {
        assert!(!all_ids().is_empty());
        assert!(groups_json().as_array().map(Vec::len).unwrap_or(0) >= 4);
    }

    #[test]
    fn known_ids_resolve_with_defaults() {
        // Stable ids the Settings page + turn driver rely on.
        for id in ["inference_bar.chat", "voice_assistant.agent_turn"] {
            let e = entry_for(id).unwrap_or_else(|| panic!("missing {id}"));
            assert_eq!(e.id, id);
            assert!(!e.default.trim().is_empty());
            assert_eq!(default_for(id), e.default);
        }
    }

    #[test]
    fn unknown_id_yields_empty_default_like_python() {
        assert!(entry_for("nope.nope").is_none());
        assert_eq!(default_for("nope.nope"), "");
    }
}
