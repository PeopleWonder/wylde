//! A graph node — the GUI-side mirror of the `workspaces.graph` verb's `Node`
//! (Slice B, `wylde-workspaces/src/graph/projection.rs`).
//!
//! The field set and serde representations match the wire shape exactly so a
//! `WorkspaceGraph` deserialises straight off the verb reply
//! (`serde_json::from_value`):
//!   * `kind` serialises as the bare PascalCase variant name (`"Function"`),
//!   * `position` is `(x, y, z)` from day one with `z == 0` in v1 (Plan v2
//!     §10) — the v2 3D upgrade is a renderer swap, not a struct rewrite,
//!   * `file` is a plain string path (the service sends a JSON string).
//!
//! This is the canonical home for `Node` (Build Order Appendix B → GUI
//! Workspaces · `graph/model/node.rs`).

use serde::{Deserialize, Serialize};

/// What a graph node *is*. Mirrors the service's `NodeKind`; `Unknown`
/// absorbs any future kind the service starts emitting so an older panel
/// still deserialises a newer graph.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum NodeKind {
    Function,
    Class,
    Trait,
    Module,
    Constant,
    Enum,
    /// User-defined concept anchor (vocabulary overlay; Slice N).
    Anchor,
    /// Forward-compat catch-all for kinds added after this panel shipped.
    #[serde(other)]
    Unknown,
}

impl NodeKind {
    /// The `node_types` key this kind maps to in the Visual Style theme
    /// (`render/style.rs`). Keeps the theme lookup in one place rather than
    /// scattering string literals through the renderer.
    pub fn theme_key(self) -> &'static str {
        match self {
            NodeKind::Function => "function",
            NodeKind::Class => "class_struct",
            NodeKind::Trait => "trait_interface",
            NodeKind::Module => "module",
            NodeKind::Constant => "constant",
            NodeKind::Enum => "enum",
            NodeKind::Anchor => "anchor_concept",
            // Treat unknown kinds as plain functions for sizing/treatment.
            NodeKind::Unknown => "function",
        }
    }
}

/// A 3D position. `z` is always `0.0` in v1 (Plan v2 §10); the field exists
/// so the 3D renderer is a drop-in swap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// Per-node rendering hints from the service. Empty in v1 — the panel
/// computes real colours/sizes from `kind` + language + theme. Present so the
/// wire shape stays stable as ingest enrichment lands.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeStyle {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub radius: Option<f32>,
}

/// A graph node. See the module docs for what is real vs. defaulted in v1.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub kind: NodeKind,
    pub name: String,
    /// Source file the entity is mentioned in (may be empty for synthesised
    /// external edge targets). A plain path string off the wire.
    #[serde(default)]
    pub file: String,
    #[serde(default)]
    pub line: u32,
    #[serde(default)]
    pub position: Position,
    #[serde(default)]
    pub style: NodeStyle,
}

impl Node {
    /// The lowercase language token for this node's file extension, matching
    /// the `language_colors` keys in the Visual Style theme. The wire graph
    /// drops the chunk language (the service keeps only `file`), so the panel
    /// re-derives it from the extension. Returns `None` for file-less nodes
    /// (external edge targets) and unrecognised extensions.
    pub fn language(&self) -> Option<&'static str> {
        language_for_path(&self.file)
    }
}

/// Map a file path to a Visual Style `language_colors` key by extension.
/// Pure + table-driven so the renderer never hardcodes a colour — it asks the
/// theme for `language_for_path(file)`'s entry.
pub fn language_for_path(path: &str) -> Option<&'static str> {
    let ext = path.rsplit('.').next().filter(|e| *e != path)?;
    Some(match ext.to_ascii_lowercase().as_str() {
        "rs" => "rust",
        "py" | "pyi" => "python",
        "ts" => "typescript",
        "js" | "mjs" | "cjs" => "javascript",
        "tsx" | "jsx" => "tsx",
        "md" | "markdown" => "markdown",
        "go" => "go",
        "json" => "json",
        "yaml" | "yml" => "yaml",
        "html" | "htm" => "html",
        "css" | "scss" | "sass" => "css",
        "sh" | "bash" | "zsh" | "ps1" => "shell",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" => "cpp",
        "java" => "java",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deserialises_service_wire_node() {
        // The exact shape `workspaces.graph` sends for one node.
        let v = json!({
            "id": "alpha",
            "kind": "Function",
            "name": "alpha",
            "file": "C:/ws/src/widget.rs",
            "line": 0,
            "position": { "x": 0.0, "y": 0.0, "z": 0.0 },
            "style": {}
        });
        let n: Node = serde_json::from_value(v).unwrap();
        assert_eq!(n.id, "alpha");
        assert_eq!(n.kind, NodeKind::Function);
        assert_eq!(n.position.z, 0.0);
        assert_eq!(n.language(), Some("rust"));
    }

    #[test]
    fn unknown_kind_falls_back_not_errors() {
        let v = json!({ "id": "x", "kind": "Macro", "name": "x" });
        let n: Node = serde_json::from_value(v).unwrap();
        assert_eq!(n.kind, NodeKind::Unknown);
        // Unknown sizes/treats as a plain function.
        assert_eq!(n.kind.theme_key(), "function");
    }

    #[test]
    fn missing_optional_fields_default() {
        let v = json!({ "id": "y", "kind": "Module", "name": "y" });
        let n: Node = serde_json::from_value(v).unwrap();
        assert!(n.file.is_empty());
        assert_eq!(n.line, 0);
        assert_eq!(n.position, Position::default());
        assert_eq!(n.language(), None, "file-less node has no language");
    }

    #[test]
    fn language_for_path_covers_theme_keys() {
        assert_eq!(language_for_path("a/b/c.rs"), Some("rust"));
        assert_eq!(language_for_path("m.py"), Some("python"));
        assert_eq!(language_for_path("Component.tsx"), Some("tsx"));
        assert_eq!(language_for_path("notes.md"), Some("markdown"));
        assert_eq!(language_for_path("x.unknownext"), None);
        assert_eq!(language_for_path("noextension"), None);
        assert_eq!(language_for_path(""), None);
    }

    #[test]
    fn kind_theme_keys_are_stable() {
        assert_eq!(NodeKind::Class.theme_key(), "class_struct");
        assert_eq!(NodeKind::Trait.theme_key(), "trait_interface");
        assert_eq!(NodeKind::Anchor.theme_key(), "anchor_concept");
    }
}
