//! Hierarchy sub-tab → pipe calls (definitional-hierarchy plan H2): the
//! per-workspace hierarchy overlay + projection on `wylde-workspaces`
//! (`workspaces.hierarchy.*`, shipped H1).
//!
//! `HierNodeView` / `TreeReply` mirror the service's wire shapes locally — the
//! same GUI-decoupling convention as [`super::super::vocabulary::concepts_ipc`]:
//! the GUI crate doesn't link the service crate, and `#[serde(default)]` keeps
//! older / partial records loading.
//!
//! Everything is behaviour-safe: with the master toggle OFF the read verbs
//! return `{enabled:false}` and the write verbs refuse, so a disabled tab is
//! inert (plan SS4).

use serde::Deserialize;
use serde_json::{json, Value};

const SVC_WORKSPACES: &str = "wylde-workspaces";

/// The GUI mirror of one node's resolved definition (`{text, source}`). `source`
/// is the priority-ladder rung: `authored | inherited_concept | inherited_anchor
/// | llm_draft | missing`.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct DefinitionView {
    pub text: String,
    pub source: String,
}

/// The GUI mirror of one projected+overlaid hierarchy node (the `HierNode` wire
/// shape from `workspaces.hierarchy.get_tree`).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct HierNodeView {
    pub id: String,
    pub label: String,
    pub definition: DefinitionView,
    /// `concept | vocab | authored`.
    pub kind: String,
    pub parents: Vec<String>,
    pub children: Vec<String>,
    pub is_leaf: bool,
}

impl HierNodeView {
    /// True when this node has no definition anywhere (browse-only; the "needs
    /// definition" invariant surfacing, plan SS3).
    pub fn needs_definition(&self) -> bool {
        self.definition.source == "missing" || self.definition.text.trim().is_empty()
    }

    /// The underlying source id (the part after the `concept:` / `vocab:` /
    /// `node:` prefix) — what a graph deep-link targets.
    pub fn source_id(&self) -> &str {
        self.id
            .split_once(':')
            .map(|(_, rest)| rest)
            .unwrap_or(&self.id)
    }
}

/// The whole `get_tree` reply.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct TreeReply {
    /// The master-toggle state. `false` ⇒ the tab renders its inert disabled
    /// state and `nodes` is empty.
    pub enabled: bool,
    pub count: usize,
    pub roots: Vec<String>,
    pub leaves: Vec<String>,
    pub nodes: Vec<HierNodeView>,
    pub dangling_count: usize,
}

/// The GUI mirror of one raw authored containment edge (with its dangling flag).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct OverlayEdgeView {
    pub parent: String,
    pub child: String,
    pub dangling: bool,
}

/// The GUI mirror of one raw authored merge (with its dangling flag).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct OverlayMergeView {
    pub primary: String,
    pub alias: String,
    pub dangling: bool,
}

/// The `get_overlay` reply — the raw authored overlay for the authoring UI.
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct OverlayReply {
    pub enabled: bool,
    pub edges: Vec<OverlayEdgeView>,
    pub merges: Vec<OverlayMergeView>,
}

async fn workspaces_call(action: &str, payload: Value) -> Result<Value, String> {
    wylde_gui_pipe::call(
        SVC_WORKSPACES,
        "POST",
        "/__action__",
        Some(json!({ "action": action, "payload": payload })),
    )
    .await
}

/// `workspaces.hierarchy.get_tree` — the whole applied DAG (or the inert
/// `enabled:false` shape when the master toggle is off).
pub async fn get_tree(ws: &str) -> Result<TreeReply, String> {
    let v = workspaces_call(
        "workspaces.hierarchy.get_tree",
        json!({ "workspace_id": ws }),
    )
    .await?;
    serde_json::from_value(v).map_err(|e| format!("bad get_tree reply: {e}"))
}

/// `workspaces.hierarchy.set_enabled` — flip the master toggle. Returns the new
/// state.
pub async fn set_enabled(enabled: bool) -> Result<bool, String> {
    let v = workspaces_call(
        "workspaces.hierarchy.set_enabled",
        json!({ "enabled": enabled }),
    )
    .await?;
    Ok(v.get("enabled").and_then(Value::as_bool).unwrap_or(enabled))
}

/// `workspaces.hierarchy.set_definition` — author/override a node's definition
/// (and optional label), or mint a brand-new authored node when `id` is `None`.
/// Returns the affected node id. (Authoring lands in H3; the call lives here so
/// the IPC surface is complete.)
pub async fn set_definition(
    ws: &str,
    id: Option<&str>,
    definition: &str,
    label: Option<&str>,
) -> Result<String, String> {
    let mut payload = json!({ "workspace_id": ws, "definition": definition });
    if let Some(id) = id {
        payload["id"] = json!(id);
    }
    if let Some(label) = label {
        payload["label"] = json!(label);
    }
    let v = workspaces_call("workspaces.hierarchy.set_definition", payload).await?;
    Ok(v.get("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned())
}

/// `workspaces.hierarchy.add_edge` — author one containment edge (H4).
pub async fn add_edge(ws: &str, parent: &str, child: &str) -> Result<(), String> {
    workspaces_call(
        "workspaces.hierarchy.add_edge",
        json!({ "workspace_id": ws, "parent": parent, "child": child }),
    )
    .await
    .map(|_| ())
}

/// `workspaces.hierarchy.remove_edge` — delete one authored containment edge (H4).
pub async fn remove_edge(ws: &str, parent: &str, child: &str) -> Result<bool, String> {
    let v = workspaces_call(
        "workspaces.hierarchy.remove_edge",
        json!({ "workspace_id": ws, "parent": parent, "child": child }),
    )
    .await?;
    Ok(v.get("removed").and_then(Value::as_bool).unwrap_or(false))
}

/// `workspaces.hierarchy.merge_nodes` — declare two nodes are one (H4).
pub async fn merge_nodes(ws: &str, primary: &str, alias: &str) -> Result<(), String> {
    workspaces_call(
        "workspaces.hierarchy.merge_nodes",
        json!({ "workspace_id": ws, "primary": primary, "alias": alias }),
    )
    .await
    .map(|_| ())
}

/// `workspaces.hierarchy.remove_merge` — undo a merge (H4); the alias re-appears.
pub async fn remove_merge(ws: &str, primary: &str, alias: &str) -> Result<bool, String> {
    let v = workspaces_call(
        "workspaces.hierarchy.remove_merge",
        json!({ "workspace_id": ws, "primary": primary, "alias": alias }),
    )
    .await?;
    Ok(v.get("removed").and_then(Value::as_bool).unwrap_or(false))
}

/// `workspaces.hierarchy.get_overlay` — the raw authored overlay (edges + merges
/// with dangling flags) for the authoring UI's re-point/remove affordances.
pub async fn get_overlay(ws: &str) -> Result<OverlayReply, String> {
    let v = workspaces_call(
        "workspaces.hierarchy.get_overlay",
        json!({ "workspace_id": ws }),
    )
    .await?;
    serde_json::from_value(v).map_err(|e| format!("bad get_overlay reply: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tree_reply_parses_the_wire_shape() {
        let v = json!({
            "enabled": true,
            "count": 2,
            "roots": ["concept:auth"],
            "leaves": ["concept:token"],
            "nodes": [
                { "id": "concept:auth", "label": "Auth",
                  "definition": { "text": "authentication", "source": "inherited_concept" },
                  "kind": "concept", "parents": [], "children": ["concept:token"], "is_leaf": false },
                { "id": "concept:token", "label": "Token",
                  "definition": { "text": "", "source": "missing" },
                  "kind": "concept", "parents": ["concept:auth"], "children": [], "is_leaf": true }
            ],
            "dangling_count": 0
        });
        let r: TreeReply = serde_json::from_value(v).unwrap();
        assert!(r.enabled);
        assert_eq!(r.count, 2);
        assert_eq!(r.nodes[0].id, "concept:auth");
        assert!(!r.nodes[0].needs_definition());
        assert!(
            r.nodes[1].needs_definition(),
            "missing source flags needs-definition"
        );
        assert_eq!(r.nodes[0].source_id(), "auth");
    }

    #[test]
    fn disabled_reply_is_inert() {
        let v = json!({ "enabled": false, "nodes": [], "count": 0 });
        let r: TreeReply = serde_json::from_value(v).unwrap();
        assert!(!r.enabled);
        assert!(r.nodes.is_empty());
    }

    #[test]
    fn source_id_handles_colon_bearing_ids() {
        let n = HierNodeView {
            id: "concept:dir:src/graph".into(),
            ..Default::default()
        };
        assert_eq!(n.source_id(), "dir:src/graph");
        let m = HierNodeView {
            id: "node:0003".into(),
            ..Default::default()
        };
        assert_eq!(m.source_id(), "0003");
    }
}
