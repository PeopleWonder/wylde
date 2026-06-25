//! The server-side seam for the **definitional concept hierarchy** overlay
//! (definitional-hierarchy plan H1) -- the deletable sibling of
//! [`super::relations_bridge`].
//!
//! This is the *only impure part* of the hierarchy: it persists the
//! per-workspace `hierarchy.json` overlay + `hierarchy_identity.json` id
//! allocator (encrypted at rest, OI-14), maps the Core [`Concept`] into the
//! crate's Core-free `ConceptView`, projects + applies the view, and hosts the
//! `workspaces.hierarchy.*` verb handlers. The hierarchy **types** (the
//! [`HierGraph`] view, the [`HierarchyOverlay`] + [`HierarchyIdentity`], the
//! pure `apply_overlay` fold) live in the isolated `wylde-concept-hierarchy`
//! crate; this file only does the I/O + validation + verb wiring so the removal
//! test holds:
//!
//! **Removal test (plan SS5):** delete this file + `hierarchy.json` +
//! `hierarchy_identity.json` + the crate + the six hierarchy verbs ⇒ Core
//! builds and the concept / anchor / relation stores are byte-identical to
//! today (an empty overlay is the projection's identity, and the projection
//! never writes).
//!
//! ## Stores
//!
//! * `<data_dir>/workspaces/<id>/hierarchy.json` -- the additive overlay
//!   ([`HierarchyOverlay`]'s serde shape), encrypted, fail-soft to empty.
//! * `<data_dir>/workspaces/<id>/hierarchy_identity.json` -- the never-reused
//!   `node:<n>` ordinal allocator, encrypted, fail-soft to default.
//!
//! The concept / anchor / relation stores stay canonical and are read-only from
//! here; the overlay holds only the net-new authored data they cannot express.
//!
//! ## Toggle gating (plan SS4, fail-closed OFF)
//!
//! Every verb consults the master [`HierarchyConfig`]. When the toggle is OFF
//! (the default), the read verbs return an inert `{enabled:false}` empty result
//! and the write verbs refuse -- so "OFF ⇒ no effect" is provable at the verb
//! layer, not merely because the (later) sub-tab is hidden.

use std::collections::HashSet;
use std::path::PathBuf;

use serde_json::{json, Value};
use wylde_concept_hierarchy::{
    apply_overlay, ConceptView, DefSource, HierGraph, HierarchyConfig, HierarchyIdentity,
    HierarchyOverlay, NodeId, NodeMerge, OverlayEdge, OverlayNode,
};
use wylde_shared::ipc::{IpcError, Reply};

use crate::anchors::store as anchor_store;
use crate::concepts::store as concept_store;
use crate::registry::persistence::workspace_dir;

// ── Paths + persistence (encrypted-at-rest, atomic, fail-soft) ───────────────

/// `<data_dir>/workspaces/<workspace_id>/hierarchy.json`.
pub fn overlay_path(workspace_id: &str) -> PathBuf {
    workspace_dir(workspace_id).join("hierarchy.json")
}

/// `<data_dir>/workspaces/<workspace_id>/hierarchy_identity.json`.
pub fn identity_path(workspace_id: &str) -> PathBuf {
    workspace_dir(workspace_id).join("hierarchy_identity.json")
}

/// Load the overlay. Fail-soft: empty on a missing/torn file. Decrypts at rest.
pub fn load_overlay(workspace_id: &str) -> HierarchyOverlay {
    let Ok(raw) = wylde_shared::encryption::read_to_string_at_rest(&overlay_path(workspace_id))
    else {
        return HierarchyOverlay::empty();
    };
    serde_json::from_str(&raw).unwrap_or_else(|_| HierarchyOverlay::empty())
}

/// Encrypt-at-rest + atomically replace `hierarchy.json`.
pub fn save_overlay(workspace_id: &str, overlay: &HierarchyOverlay) -> std::io::Result<()> {
    let body = serde_json::to_string_pretty(overlay).unwrap();
    wylde_shared::encryption::write_at_rest(&overlay_path(workspace_id), body.as_bytes())
}

/// Load the id allocator. Fail-soft: default (`next_node_ordinal: 0`).
pub fn load_identity(workspace_id: &str) -> HierarchyIdentity {
    let Ok(raw) = wylde_shared::encryption::read_to_string_at_rest(&identity_path(workspace_id))
    else {
        return HierarchyIdentity::default();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Encrypt-at-rest + atomically replace `hierarchy_identity.json`.
pub fn save_identity(workspace_id: &str, identity: &HierarchyIdentity) -> std::io::Result<()> {
    let body = serde_json::to_string_pretty(identity).unwrap();
    wylde_shared::encryption::write_at_rest(&identity_path(workspace_id), body.as_bytes())
}

// ── Projection (Concept -> ConceptView) ──────────────────────────────────────

/// Map every stored [`Concept`] into the crate's Core-free [`ConceptView`] --
/// the seam that keeps `wylde-concept-hierarchy` from ever depending on Core
/// (plan SS2.1 "Why a `ConceptView` and not `Concept`").
fn concept_views(workspace_id: &str) -> Vec<ConceptView> {
    concept_store::load(workspace_id)
        .into_iter()
        .map(|c| ConceptView {
            id: c.id,
            label: c.label,
            description: c.description,
            parent_concepts: c.parent_concepts,
            described_by: c.described_by,
            centroid: c.centroid,
        })
        .collect()
}

/// The PROJECTED base graph (no overlay) -- the DAG the existing stores already
/// draw. The universe an overlay edge / merge / definition is validated against.
fn base_graph(workspace_id: &str) -> HierGraph {
    wylde_concept_hierarchy::build_view(
        &concept_views(workspace_id),
        &anchor_store::load(workspace_id),
        &super::relations_bridge::load(workspace_id),
    )
}

/// The full APPLIED graph: the projection with the overlay folded in. This is
/// what the read verbs surface and what traversal runs over.
pub fn current_graph(workspace_id: &str) -> HierGraph {
    apply_overlay(base_graph(workspace_id), &load_overlay(workspace_id))
}

/// Does this id refer to a real node? -- present in the projected base OR
/// recorded as an authored overlay node. (Checked against the base, NOT the
/// applied graph, so a merge's alias still resolves before it is folded away.)
fn resolves(id: &NodeId, base: &HierGraph, overlay: &HierarchyOverlay) -> bool {
    base.contains(id) || overlay.nodes.iter().any(|n| &n.id == id)
}

/// Re-validate every overlay edge + merge after a concept recompute (the
/// `Relation.dangling` rule, plan SS2.2): flag a record `dangling` when an
/// endpoint no longer resolves, clear it when the endpoint returns. **Never
/// deletes** -- the authored record is retained on disk (surfaced for
/// re-point) but excluded from the applied graph. Returns the dangling count
/// after the sweep. No-op (saves nothing) on an empty overlay or no change.
pub fn sweep_dangling(workspace_id: &str) -> usize {
    let mut overlay = load_overlay(workspace_id);
    if overlay.is_empty() {
        return 0;
    }
    let base = base_graph(workspace_id);
    let valid: HashSet<NodeId> = base
        .nodes
        .iter()
        .map(|n| n.id.clone())
        .chain(overlay.nodes.iter().map(|n| n.id.clone()))
        .collect();

    let mut changed = false;
    let mut dangling = 0usize;
    for e in &mut overlay.edges {
        let now = !(valid.contains(&e.parent) && valid.contains(&e.child));
        if e.dangling != now {
            e.dangling = now;
            changed = true;
        }
        if now {
            dangling += 1;
        }
    }
    for m in &mut overlay.merges {
        let now = !(valid.contains(&m.primary) && valid.contains(&m.alias));
        if m.dangling != now {
            m.dangling = now;
            changed = true;
        }
        if now {
            dangling += 1;
        }
    }
    if changed {
        if let Err(e) = save_overlay(workspace_id, &overlay) {
            tracing::warn!("workspaces.hierarchy: sweep_dangling save failed for {workspace_id}: {e}");
        }
    }
    dangling
}

// ── Payload parsing helpers (Err message is always a bad_request) ────────────

fn require_ws(payload: &Value) -> Result<String, String> {
    payload
        .get("workspace_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "workspace_id is required".to_owned())
}

/// Parse a required `NodeId` from a string-valued payload key.
fn parse_node(payload: &Value, key: &str) -> Result<NodeId, String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| NodeId(s.to_owned()))
        .ok_or_else(|| format!("{key} is required (a node id string)"))
}

/// The hierarchy disabled-state reply for a write verb.
fn disabled_err() -> Reply {
    Reply::err_msg("disabled", "the concept hierarchy is disabled")
}

// ── Node serialisation (rich, for the UI) ────────────────────────────────────

/// A compact JSON view of one node: id, label, kind, the definition (text +
/// source), and the leaf flag. The definition `source` is exactly the priority
/// ladder rung the UI shows ("authored" / "inherited_*" / "missing").
fn node_summary(graph: &HierGraph, id: &NodeId) -> Value {
    match graph.node(id) {
        Some(n) => json!({
            "id": n.id,
            "label": n.label,
            "kind": n.kind,
            "definition": n.definition.text,
            "definition_source": n.definition.source,
            "needs_definition": n.definition.is_missing(),
            "is_leaf": n.is_leaf,
        }),
        // A dangling reference (id not in the applied graph): surface the bare id.
        None => json!({ "id": id, "label": id.as_str(), "missing": true }),
    }
}

// ── Verb handlers ────────────────────────────────────────────────────────────

/// `workspaces.hierarchy.get_tree` -- the whole applied [`HierGraph`] (the
/// projection + overlay), plus the roots and dangling count for the tree view.
/// Payload: `{workspace_id}`. OFF ⇒ `{enabled:false, nodes:[], ...}`.
pub async fn handle_get_tree(payload: Value) -> Reply {
    let ws = match require_ws(&payload) {
        Ok(w) => w,
        Err(m) => return Reply::err_msg("bad_request", m),
    };
    if !HierarchyConfig::current().enabled {
        return Reply::ok(json!({
            "workspace_id": ws,
            "enabled": false,
            "nodes": [],
            "xrefs": [],
            "roots": [],
            "count": 0,
            "dangling_count": 0,
        }));
    }
    // Re-validate dangling overlay records first, so the tree never shows an
    // edge to a vanished node (retained-but-excluded).
    let dangling_count = sweep_dangling(&ws);
    let graph = current_graph(&ws);
    Reply::ok(json!({
        "workspace_id": ws,
        "enabled": true,
        "count": graph.nodes.len(),
        "roots": graph.roots(),
        "leaves": graph.leaves(),
        "nodes": graph.nodes,
        "xrefs": graph.xrefs,
        "dangling_count": dangling_count,
    }))
}

/// `workspaces.hierarchy.get_node` -- one node with its parents, children, the
/// definitional ancestor-chain (the future injection payload), and the
/// cross-references touching it. Payload: `{workspace_id, id}`. OFF ⇒
/// `{enabled:false, node:null}`.
pub async fn handle_get_node(payload: Value) -> Reply {
    let ws = match require_ws(&payload) {
        Ok(w) => w,
        Err(m) => return Reply::err_msg("bad_request", m),
    };
    let id = match parse_node(&payload, "id") {
        Ok(n) => n,
        Err(m) => return Reply::err_msg("bad_request", m),
    };
    if !HierarchyConfig::current().enabled {
        return Reply::ok(json!({ "workspace_id": ws, "enabled": false, "node": Value::Null }));
    }
    let graph = current_graph(&ws);
    if graph.node(&id).is_none() {
        return Reply::err_msg("not_found", format!("no node {}", id.as_str()));
    }
    let parents: Vec<Value> = graph.parents_of(&id).iter().map(|p| node_summary(&graph, p)).collect();
    let children: Vec<Value> = graph.children_of(&id).iter().map(|c| node_summary(&graph, c)).collect();
    // The definitional ancestor chain, nearest-first (start at index 0), each
    // resolved to its definition -- the "leaf — under category — under root"
    // payload H5 will inject.
    let chain: Vec<Value> = graph.ancestor_chain(&id).iter().map(|a| node_summary(&graph, a)).collect();
    let xrefs: Vec<Value> = graph
        .xrefs
        .iter()
        .filter(|x| x.from == id || x.to == id)
        .map(|x| json!({ "from": x.from, "to": x.to, "kind": x.kind }))
        .collect();
    Reply::ok(json!({
        "workspace_id": ws,
        "enabled": true,
        "node": node_summary(&graph, &id),
        "parents": parents,
        "children": children,
        "ancestor_chain": chain,
        "xrefs": xrefs,
    }))
}

/// `workspaces.hierarchy.set_definition` -- author/override a node's definition
/// (and optionally its label), OR introduce a brand-new authored node.
///
/// Payload: `{workspace_id, id?, definition?, source?=authored|llm_draft, label?}`.
/// * With `id`: it must resolve (a projected or authored node). A non-empty
///   `definition` sets the authored override; an empty `definition` CLEARS it
///   (the node falls back to its inherited description). For a projected node
///   whose override becomes empty (no def + no label), the overlay record is
///   pruned, so the removal-test ground state is reachable.
/// * Without `id`: mints a never-reused `node:<n>` id and creates an authored
///   node; a non-empty `definition` is required.
///
/// OFF ⇒ `disabled`.
pub async fn handle_set_definition(payload: Value) -> Reply {
    let ws = match require_ws(&payload) {
        Ok(w) => w,
        Err(m) => return Reply::err_msg("bad_request", m),
    };
    if !HierarchyConfig::current().enabled {
        return disabled_err();
    }
    let definition = payload.get("definition").and_then(Value::as_str).map(str::to_owned);
    let label = payload
        .get("label")
        .and_then(Value::as_str)
        .map(str::trim)
        .map(str::to_owned);
    let source = match payload.get("source").and_then(Value::as_str) {
        Some("llm_draft") => DefSource::LlmDraft,
        Some("authored") | None => DefSource::Authored,
        Some(other) => {
            return Reply::err_msg("bad_request", format!("unknown source `{other}` (authored|llm_draft)"))
        }
    };

    let base = base_graph(&ws);
    let mut overlay = load_overlay(&ws);
    let now = wylde_shared::anchor::epoch_now();

    let id = match payload.get("id").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
        Some(s) => {
            let id = NodeId(s.to_owned());
            if !resolves(&id, &base, &overlay) {
                return Reply::err_msg("bad_request", format!("unknown node: {}", id.as_str()));
            }
            id
        }
        None => {
            // Creating a brand-new authored node: a definition is required.
            if definition.as_deref().map(str::trim).unwrap_or("").is_empty() {
                return Reply::err_msg("bad_request", "a new node requires a non-empty `definition`");
            }
            let mut identity = load_identity(&ws);
            let id = identity.mint();
            if let Err(e) = save_identity(&ws, &identity) {
                return Reply::err_msg("io", format!("failed to persist id allocator: {e}"));
            }
            id
        }
    };

    // Upsert the overlay node.
    let is_base_node = base.contains(&id);
    match overlay.nodes.iter_mut().find(|n| n.id == id) {
        Some(existing) => {
            if payload.get("definition").is_some() {
                existing.definition = definition
                    .as_deref()
                    .filter(|d| !d.trim().is_empty())
                    .map(str::to_owned);
                existing.definition_source = source;
            }
            if let Some(l) = &label {
                existing.label_override = if l.is_empty() { None } else { Some(l.clone()) };
            }
            existing.updated_at = now;
        }
        None => overlay.nodes.push(OverlayNode {
            id: id.clone(),
            definition: definition.as_deref().filter(|d| !d.trim().is_empty()).map(str::to_owned),
            definition_source: source,
            label_override: label.as_deref().filter(|l| !l.is_empty()).map(str::to_owned),
            created_at: now,
            updated_at: now,
        }),
    }

    // Prune an emptied override of a projected node (revert to inherited). An
    // authored-only node is kept even when empty -- it is still a real node.
    if is_base_node {
        if let Some(pos) = overlay.nodes.iter().position(|n| n.id == id) {
            if overlay.nodes[pos].is_empty() {
                overlay.nodes.remove(pos);
            }
        }
    }

    if let Err(e) = save_overlay(&ws, &overlay) {
        return Reply::err_msg("io", format!("failed to persist overlay: {e}"));
    }
    let graph = apply_overlay(base, &overlay);
    Reply::ok(json!({ "workspace_id": ws, "id": id, "node": node_summary(&graph, &id) }))
}

/// `workspaces.hierarchy.add_edge` -- author one containment edge
/// (`parent contains child`). Payload: `{workspace_id, parent, child}`.
/// Re-adding a `dangling` edge clears its flag. Rejects a self-edge / unknown
/// endpoint; `already_exists` on a live duplicate. OFF ⇒ `disabled`.
pub async fn handle_add_edge(payload: Value) -> Reply {
    let ws = match require_ws(&payload) {
        Ok(w) => w,
        Err(m) => return Reply::err_msg("bad_request", m),
    };
    if !HierarchyConfig::current().enabled {
        return disabled_err();
    }
    let parent = match parse_node(&payload, "parent") {
        Ok(n) => n,
        Err(m) => return Reply::err_msg("bad_request", m),
    };
    let child = match parse_node(&payload, "child") {
        Ok(n) => n,
        Err(m) => return Reply::err_msg("bad_request", m),
    };
    if parent == child {
        return Reply::err_msg("bad_request", "a containment edge cannot connect a node to itself");
    }
    let base = base_graph(&ws);
    let mut overlay = load_overlay(&ws);
    if !resolves(&parent, &base, &overlay) {
        return Reply::err_msg("bad_request", format!("unknown parent node: {}", parent.as_str()));
    }
    if !resolves(&child, &base, &overlay) {
        return Reply::err_msg("bad_request", format!("unknown child node: {}", child.as_str()));
    }
    let now = wylde_shared::anchor::epoch_now();
    let probe = OverlayEdge { parent: parent.clone(), child: child.clone(), created_at: now, dangling: false };
    if let Some(existing) = overlay.edges.iter_mut().find(|e| e.same_edge(&probe)) {
        if existing.dangling {
            existing.dangling = false; // re-point: bring a retained edge back
            if let Err(e) = save_overlay(&ws, &overlay) {
                return Reply::err_msg("io", format!("failed to persist overlay: {e}"));
            }
            return Reply::ok(json!({ "workspace_id": ws, "reactivated": true, "edge": probe }));
        }
        return Reply::err(IpcError {
            code: "already_exists".into(),
            message: "this containment edge already exists".into(),
            details: Some(json!({ "edge": existing })),
        });
    }
    overlay.edges.push(probe.clone());
    if let Err(e) = save_overlay(&ws, &overlay) {
        return Reply::err_msg("io", format!("failed to persist overlay: {e}"));
    }
    Reply::ok(json!({ "workspace_id": ws, "edge": probe }))
}

/// `workspaces.hierarchy.remove_edge` -- delete one authored containment edge.
/// Payload: `{workspace_id, parent, child}`. Reply: `{removed: bool}`. Only
/// overlay edges are removable; the projection's own edges live in the concept
/// store. OFF ⇒ `disabled`.
pub async fn handle_remove_edge(payload: Value) -> Reply {
    let ws = match require_ws(&payload) {
        Ok(w) => w,
        Err(m) => return Reply::err_msg("bad_request", m),
    };
    if !HierarchyConfig::current().enabled {
        return disabled_err();
    }
    let parent = match parse_node(&payload, "parent") {
        Ok(n) => n,
        Err(m) => return Reply::err_msg("bad_request", m),
    };
    let child = match parse_node(&payload, "child") {
        Ok(n) => n,
        Err(m) => return Reply::err_msg("bad_request", m),
    };
    let target = OverlayEdge { parent, child, created_at: 0.0, dangling: false };
    let mut overlay = load_overlay(&ws);
    let before = overlay.edges.len();
    overlay.edges.retain(|e| !e.same_edge(&target));
    let removed = overlay.edges.len() != before;
    if removed {
        if let Err(e) = save_overlay(&ws, &overlay) {
            return Reply::err_msg("io", format!("failed to persist overlay: {e}"));
        }
    }
    Reply::ok(json!({ "workspace_id": ws, "removed": removed }))
}

/// `workspaces.hierarchy.merge_nodes` -- declare two nodes are one (OQ-2). The
/// `alias` folds into the `primary` on apply. Payload:
/// `{workspace_id, primary, alias}`. Rejects a self-merge / unknown endpoint;
/// `already_exists` on a live duplicate; re-adding a dangling merge clears its
/// flag. OFF ⇒ `disabled`.
pub async fn handle_merge_nodes(payload: Value) -> Reply {
    let ws = match require_ws(&payload) {
        Ok(w) => w,
        Err(m) => return Reply::err_msg("bad_request", m),
    };
    if !HierarchyConfig::current().enabled {
        return disabled_err();
    }
    let primary = match parse_node(&payload, "primary") {
        Ok(n) => n,
        Err(m) => return Reply::err_msg("bad_request", m),
    };
    let alias = match parse_node(&payload, "alias") {
        Ok(n) => n,
        Err(m) => return Reply::err_msg("bad_request", m),
    };
    if primary == alias {
        return Reply::err_msg("bad_request", "cannot merge a node with itself");
    }
    let base = base_graph(&ws);
    let mut overlay = load_overlay(&ws);
    if !resolves(&primary, &base, &overlay) {
        return Reply::err_msg("bad_request", format!("unknown primary node: {}", primary.as_str()));
    }
    if !resolves(&alias, &base, &overlay) {
        return Reply::err_msg("bad_request", format!("unknown alias node: {}", alias.as_str()));
    }
    let now = wylde_shared::anchor::epoch_now();
    let probe = NodeMerge { primary: primary.clone(), alias: alias.clone(), created_at: now, dangling: false };
    if let Some(existing) = overlay.merges.iter_mut().find(|m| m.same_merge(&probe)) {
        if existing.dangling {
            existing.dangling = false;
            if let Err(e) = save_overlay(&ws, &overlay) {
                return Reply::err_msg("io", format!("failed to persist overlay: {e}"));
            }
            return Reply::ok(json!({ "workspace_id": ws, "reactivated": true, "merge": probe }));
        }
        return Reply::err(IpcError {
            code: "already_exists".into(),
            message: "this merge already exists".into(),
            details: Some(json!({ "merge": existing })),
        });
    }
    overlay.merges.push(probe.clone());
    if let Err(e) = save_overlay(&ws, &overlay) {
        return Reply::err_msg("io", format!("failed to persist overlay: {e}"));
    }
    Reply::ok(json!({ "workspace_id": ws, "merge": probe }))
}

/// `workspaces.hierarchy.remove_merge` -- undo a merge by `(primary, alias)`,
/// so the alias re-appears as its own node (authoring stays reversible).
/// Payload: `{workspace_id, primary, alias}`. Reply: `{removed: bool}`. OFF ⇒
/// `disabled`.
pub async fn handle_remove_merge(payload: Value) -> Reply {
    let ws = match require_ws(&payload) {
        Ok(w) => w,
        Err(m) => return Reply::err_msg("bad_request", m),
    };
    if !HierarchyConfig::current().enabled {
        return disabled_err();
    }
    let primary = match parse_node(&payload, "primary") {
        Ok(n) => n,
        Err(m) => return Reply::err_msg("bad_request", m),
    };
    let alias = match parse_node(&payload, "alias") {
        Ok(n) => n,
        Err(m) => return Reply::err_msg("bad_request", m),
    };
    let target = NodeMerge { primary, alias, created_at: 0.0, dangling: false };
    let mut overlay = load_overlay(&ws);
    let before = overlay.merges.len();
    overlay.merges.retain(|m| !m.same_merge(&target));
    let removed = overlay.merges.len() != before;
    if removed {
        if let Err(e) = save_overlay(&ws, &overlay) {
            return Reply::err_msg("io", format!("failed to persist overlay: {e}"));
        }
    }
    Reply::ok(json!({ "workspace_id": ws, "removed": removed }))
}

/// `workspaces.hierarchy.get_overlay` -- the RAW authored overlay (authored
/// nodes, containment edges, merges) WITH their `dangling` flags, for the
/// authoring UI. Unlike `get_tree` (which folds + excludes dangling records),
/// this surfaces them so the UI can offer re-point / remove. Payload:
/// `{workspace_id}`. OFF ⇒ `{enabled:false, edges:[], merges:[], nodes:[]}`.
pub async fn handle_get_overlay(payload: Value) -> Reply {
    let ws = match require_ws(&payload) {
        Ok(w) => w,
        Err(m) => return Reply::err_msg("bad_request", m),
    };
    if !HierarchyConfig::current().enabled {
        return Reply::ok(json!({
            "workspace_id": ws, "enabled": false, "nodes": [], "edges": [], "merges": [],
        }));
    }
    // Refresh dangling flags first so the UI sees the current state.
    sweep_dangling(&ws);
    let overlay = load_overlay(&ws);
    Reply::ok(json!({
        "workspace_id": ws,
        "enabled": true,
        "nodes": overlay.nodes,
        "edges": overlay.edges,
        "merges": overlay.merges,
    }))
}

// ── Master toggle facade (OQ-7 default: one toggle) ──────────────────────────
//
// Ungated -- these are how the sub-tab reads + flips the master switch, so they
// MUST work while the feature is off (otherwise it could never be turned on).
// `set_enabled` persists through `HierarchyConfig`, updating both this service's
// in-memory cache and `<data_dir>/settings/hierarchy.json` (fail-closed OFF).

/// `workspaces.hierarchy.get_config` -- the master toggle state. Payload: `{}`.
/// Reply: `{enabled}`.
pub async fn handle_get_config(_payload: Value) -> Reply {
    Reply::ok(json!({ "enabled": HierarchyConfig::current().enabled }))
}

/// `workspaces.hierarchy.set_enabled` -- flip the master toggle. Payload:
/// `{enabled: bool}`. Reply: `{enabled}`. A persist failure still updates the
/// in-session cache (optimistic) and is surfaced as `io`.
pub async fn handle_set_enabled(payload: Value) -> Reply {
    let Some(enabled) = payload.get("enabled").and_then(Value::as_bool) else {
        return Reply::err_msg("bad_request", "enabled (bool) is required");
    };
    match HierarchyConfig::persist(HierarchyConfig { enabled }) {
        Ok(()) => Reply::ok(json!({ "enabled": enabled })),
        Err(e) => Reply::err_msg("io", format!("toggle saved in-session but disk write failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::concepts::concept::{Concept, ConceptSource};
    use crate::test_support::TestEnv;
    use wylde_shared::anchor::{Anchor, AnchorKind, AnchorScope, AnchorTarget};

    /// Turn the master toggle ON for a test (updates the process-global cache
    /// directly, so it is deterministic regardless of the on-disk file).
    fn enable() {
        HierarchyConfig::persist(HierarchyConfig { enabled: true }).expect("enable");
    }
    /// Restore the toggle to its OFF default after a test.
    fn disable() {
        let _ = HierarchyConfig::persist(HierarchyConfig { enabled: false });
    }

    fn vocab(ws: &str, identifier: &str, parent: Option<&str>) -> Anchor {
        let mut a = Anchor::new(
            identifier,
            AnchorKind::Concept,
            AnchorTarget::Concept { text: identifier.into() },
            AnchorScope::Workspace { workspace_id: ws.into() },
            format!("the {identifier}"),
        );
        a.parent_anchor = parent.map(str::to_owned);
        a
    }

    /// Two concepts (one a child of the other) + two vocab anchors.
    fn seed(ws: &str) {
        concept_store::save(
            ws,
            &[
                Concept::new("auth", "Auth", "authentication", ConceptSource::Manual),
                {
                    let mut c = Concept::new("token", "Token", "a bearer credential", ConceptSource::Manual);
                    c.parent_concepts = vec!["auth".into()];
                    c
                },
            ],
        )
        .unwrap();
        anchor_store::save(ws, &[vocab(ws, "n8n", None), vocab(ws, "workflows", None)]).unwrap();
    }

    #[tokio::test]
    async fn get_tree_projects_the_existing_dag() {
        let _env = TestEnv::new();
        enable();
        let ws = "hier-tree-0000";
        seed(ws);
        let r = handle_get_tree(json!({ "workspace_id": ws })).await;
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data["enabled"], json!(true));
        // 2 concepts + 2 vocab = 4 nodes; `auth` is a root, `token` a leaf.
        assert_eq!(r.data["count"], json!(4));
        let roots = r.data["roots"].as_array().unwrap();
        assert!(roots.iter().any(|v| v == "concept:auth"));
        disable();
    }

    #[tokio::test]
    async fn toggle_off_is_inert_at_the_verb_layer() {
        let _env = TestEnv::new();
        disable(); // master OFF
        let ws = "hier-off-00000";
        seed(ws);
        // Read verbs return the inert empty shape.
        let tree = handle_get_tree(json!({ "workspace_id": ws })).await;
        assert!(tree.ok);
        assert_eq!(tree.data["enabled"], json!(false));
        assert_eq!(tree.data["count"], json!(0));
        // Write verbs refuse.
        let set = handle_set_definition(json!({
            "workspace_id": ws, "id": "concept:auth", "definition": "x"
        }))
        .await;
        assert!(!set.ok);
        assert_eq!(set.error.unwrap().code, "disabled");
        // And nothing was written -- the overlay file stays absent/empty.
        assert!(load_overlay(ws).is_empty(), "OFF wrote nothing");
    }

    #[tokio::test]
    async fn set_definition_authors_and_persists_override() {
        let _env = TestEnv::new();
        enable();
        let ws = "hier-def-00000";
        seed(ws);
        // Override `auth`'s inherited definition.
        let r = handle_set_definition(json!({
            "workspace_id": ws, "id": "concept:auth", "definition": "hand-written auth def"
        }))
        .await;
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data["node"]["definition_source"], json!("authored"));
        assert_eq!(r.data["node"]["definition"], json!("hand-written auth def"));
        // Persisted: a fresh applied graph still shows the override.
        let g = current_graph(ws);
        let n = g.node(&NodeId::concept("auth")).unwrap();
        assert_eq!(n.definition.source, DefSource::Authored);

        // Clearing it reverts to the inherited description AND prunes the record.
        let clear = handle_set_definition(json!({
            "workspace_id": ws, "id": "concept:auth", "definition": ""
        }))
        .await;
        assert!(clear.ok);
        assert_eq!(clear.data["node"]["definition_source"], json!("inherited_concept"));
        assert!(load_overlay(ws).is_empty(), "emptied override pruned to ground state");
        disable();
    }

    #[tokio::test]
    async fn set_definition_mints_authored_node() {
        let _env = TestEnv::new();
        enable();
        let ws = "hier-new-00000";
        seed(ws);
        let r = handle_set_definition(json!({
            "workspace_id": ws, "definition": "a net-new theme", "label": "Theme"
        }))
        .await;
        assert!(r.ok, "{:?}", r.error);
        let id = r.data["id"].as_str().unwrap().to_owned();
        assert_eq!(id, "node:0000", "first minted id");
        assert_eq!(r.data["node"]["kind"], json!("authored"));
        // The allocator advanced + persisted (never reused).
        assert_eq!(load_identity(ws).next_node_ordinal, 1);
        // A second mint gets the next ordinal.
        let r2 = handle_set_definition(json!({ "workspace_id": ws, "definition": "another" })).await;
        assert_eq!(r2.data["id"], json!("node:0001"));
        disable();
    }

    #[tokio::test]
    async fn add_remove_edge_round_trip() {
        let _env = TestEnv::new();
        enable();
        let ws = "hier-edge-0000";
        seed(ws);
        // Author: n8n contains workflows (neither anchor had a parent_anchor).
        let add = handle_add_edge(json!({
            "workspace_id": ws, "parent": "vocab:n8n", "child": "vocab:workflows"
        }))
        .await;
        assert!(add.ok, "{:?}", add.error);
        // Reflected in the applied graph.
        let g = current_graph(ws);
        assert!(g
            .children_of(&NodeId::vocab("n8n"))
            .contains(&NodeId::vocab("workflows")));
        // Duplicate is already_exists.
        let dup = handle_add_edge(json!({
            "workspace_id": ws, "parent": "vocab:n8n", "child": "vocab:workflows"
        }))
        .await;
        assert_eq!(dup.error.unwrap().code, "already_exists");
        // Self-edge + unknown endpoint rejected.
        assert!(!handle_add_edge(json!({ "workspace_id": ws, "parent": "vocab:n8n", "child": "vocab:n8n" })).await.ok);
        assert!(!handle_add_edge(json!({ "workspace_id": ws, "parent": "vocab:n8n", "child": "vocab:ghost" })).await.ok);
        // Remove it.
        let rm = handle_remove_edge(json!({
            "workspace_id": ws, "parent": "vocab:n8n", "child": "vocab:workflows"
        }))
        .await;
        assert!(rm.ok && rm.data["removed"] == json!(true));
        assert!(load_overlay(ws).edges.is_empty());
        disable();
    }

    #[tokio::test]
    async fn merge_nodes_folds_alias() {
        let _env = TestEnv::new();
        enable();
        let ws = "hier-merge-000";
        seed(ws);
        // Merge the `workflows` vocab into the `auth` concept (contrived but valid).
        let m = handle_merge_nodes(json!({
            "workspace_id": ws, "primary": "concept:auth", "alias": "vocab:workflows"
        }))
        .await;
        assert!(m.ok, "{:?}", m.error);
        let g = current_graph(ws);
        assert!(g.node(&NodeId::vocab("workflows")).is_none(), "alias folded away");
        // Duplicate merge -> already_exists.
        let dup = handle_merge_nodes(json!({
            "workspace_id": ws, "primary": "concept:auth", "alias": "vocab:workflows"
        }))
        .await;
        assert_eq!(dup.error.unwrap().code, "already_exists");

        // remove_merge undoes it: the alias re-appears as its own node.
        let rm = handle_remove_merge(json!({
            "workspace_id": ws, "primary": "concept:auth", "alias": "vocab:workflows"
        }))
        .await;
        assert!(rm.ok && rm.data["removed"] == json!(true));
        let g = current_graph(ws);
        assert!(g.node(&NodeId::vocab("workflows")).is_some(), "alias restored after unmerge");
        disable();
    }

    #[tokio::test]
    async fn dangling_retained_and_excluded_then_cleared() {
        let _env = TestEnv::new();
        enable();
        let ws = "hier-dangle-00";
        seed(ws);
        // Author an edge from a concept to a vocab term.
        handle_add_edge(json!({
            "workspace_id": ws, "parent": "concept:auth", "child": "vocab:workflows"
        }))
        .await;
        assert_eq!(sweep_dangling(ws), 0, "both endpoints resolve");

        // A recompute drops the `workflows` anchor (endpoint vanishes).
        anchor_store::save(ws, &[vocab(ws, "n8n", None)]).unwrap();
        assert_eq!(sweep_dangling(ws), 1, "edge to a vanished node flagged");
        // RETAINED on disk, not deleted...
        let overlay = load_overlay(ws);
        assert_eq!(overlay.edges.len(), 1);
        assert!(overlay.edges[0].dangling);
        // ...and excluded from the applied graph (the projection's own
        // concept->concept child `token` remains; only the dangling authored
        // edge to the vanished vocab term is gone).
        let g = current_graph(ws);
        assert!(
            !g.children_of(&NodeId::concept("auth")).contains(&NodeId::vocab("workflows")),
            "dangling edge absent from traversal"
        );
        assert!(g.node(&NodeId::vocab("workflows")).is_none(), "vanished node absent too");

        // The anchor returns -> the flag clears.
        anchor_store::save(ws, &[vocab(ws, "n8n", None), vocab(ws, "workflows", None)]).unwrap();
        assert_eq!(sweep_dangling(ws), 0, "flag cleared when endpoint returns");
        assert!(!load_overlay(ws).edges[0].dangling);
        disable();
    }

    #[tokio::test]
    async fn authored_data_survives_a_simulated_recompute() {
        let _env = TestEnv::new();
        enable();
        let ws = "hier-stable-00";
        seed(ws);
        // Author an override on `auth` (stable concept id) + a fresh authored node.
        handle_set_definition(json!({
            "workspace_id": ws, "id": "concept:auth", "definition": "stable override"
        }))
        .await;
        let created = handle_set_definition(json!({ "workspace_id": ws, "definition": "authored leaf" })).await;
        let authored_id = created.data["id"].as_str().unwrap().to_owned();

        // Simulate a concept recompute: rebuild the concept set with the SAME
        // stable ids (what stable-id carry-over guarantees), different metadata.
        concept_store::save(
            ws,
            &[
                Concept::new("auth", "Auth (rebuilt)", "fresh inherited text", ConceptSource::Embedding),
                {
                    let mut c = Concept::new("token", "Token", "x", ConceptSource::Embedding);
                    c.parent_concepts = vec!["auth".into()];
                    c
                },
            ],
        )
        .unwrap();

        // The authored override re-binds to `concept:auth` (still wins the ladder).
        let g = current_graph(ws);
        let auth = g.node(&NodeId::concept("auth")).unwrap();
        assert_eq!(auth.definition.text, "stable override", "override survived recompute");
        assert_eq!(auth.definition.source, DefSource::Authored);
        // The authored node persists with its minted id untouched.
        assert!(g.node(&NodeId(authored_id.clone())).is_some(), "authored node survived");
        disable();
    }

    #[tokio::test]
    async fn get_overlay_surfaces_authored_and_dangling_records() {
        let _env = TestEnv::new();
        enable();
        let ws = "hier-ovl-00000";
        seed(ws);
        // Author an edge, then drop the child anchor so the edge dangles.
        handle_add_edge(json!({
            "workspace_id": ws, "parent": "concept:auth", "child": "vocab:workflows"
        }))
        .await;
        anchor_store::save(ws, &[vocab(ws, "n8n", None)]).unwrap();

        let r = handle_get_overlay(json!({ "workspace_id": ws })).await;
        assert!(r.ok, "{:?}", r.error);
        assert_eq!(r.data["enabled"], json!(true));
        let edges = r.data["edges"].as_array().unwrap();
        assert_eq!(edges.len(), 1, "the authored edge is RETAINED + surfaced");
        assert_eq!(edges[0]["dangling"], json!(true), "and flagged dangling for re-point");

        // OFF ⇒ inert empty.
        disable();
        let off = handle_get_overlay(json!({ "workspace_id": ws })).await;
        assert_eq!(off.data["enabled"], json!(false));
        assert!(off.data["edges"].as_array().unwrap().is_empty());
    }

    #[tokio::test]
    async fn config_toggle_facade_round_trips() {
        let _env = TestEnv::new();
        disable();
        // get_config reflects OFF.
        let c = handle_get_config(json!({})).await;
        assert_eq!(c.data["enabled"], json!(false));
        // set_enabled flips it ON, persisted to the cache.
        let s = handle_set_enabled(json!({ "enabled": true })).await;
        assert!(s.ok);
        assert_eq!(s.data["enabled"], json!(true));
        assert!(HierarchyConfig::current().enabled);
        // A read verb is now live.
        assert_eq!(
            handle_get_tree(json!({ "workspace_id": "hier-cfg-00000" })).await.data["enabled"],
            json!(true)
        );
        // Bad payload rejected.
        assert!(!handle_set_enabled(json!({})).await.ok);
        disable();
    }

    #[tokio::test]
    async fn get_node_returns_ancestor_chain() {
        let _env = TestEnv::new();
        enable();
        let ws = "hier-node-0000";
        seed(ws);
        let r = handle_get_node(json!({ "workspace_id": ws, "id": "concept:token" })).await;
        assert!(r.ok, "{:?}", r.error);
        // token -> auth, so the chain is [token, auth].
        let chain = r.data["ancestor_chain"].as_array().unwrap();
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0]["id"], json!("concept:token"));
        assert_eq!(chain[1]["id"], json!("concept:auth"));
        // Unknown node -> not_found.
        let miss = handle_get_node(json!({ "workspace_id": ws, "id": "concept:ghost" })).await;
        assert_eq!(miss.error.unwrap().code, "not_found");
        disable();
    }
}
