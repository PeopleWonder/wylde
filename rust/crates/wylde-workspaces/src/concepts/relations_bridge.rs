//! The server-side seam for the **typed relation store** (concept-routing
//! R1.5a, relation-model addendum §1.4 / §2.1) — the deletable sibling of
//! [`super::routing_bridge`].
//!
//! This is the *only impure part* of the relation model: it persists the
//! per-workspace `concept_relations.json` (encrypted at rest, OI-14) and hosts
//! the `workspaces.concepts.relations.*` verb handlers. The relation **types**
//! ([`Relation`], [`NodeRef`], [`RelationKind`], [`RelationGraph`]) are pure and
//! live in the isolated `wylde-concept-routing` crate; this file only does the
//! I/O + validation + verb wiring so the removal test holds:
//!
//! **Removal test:** delete this file + `concept_relations.json` + the crate's
//! `relations/` module + the four relation verbs ⇒ Core builds and routing
//! falls back to pure-seed R1 (an empty relation graph is the engine's
//! identity).
//!
//! ## Store
//!
//! `<data_dir>/workspaces/<workspace_id>/concept_relations.json` — a single
//! JSON object `{ "relations": [ … ] }` ([`RelationGraph`]'s serde shape),
//! encrypted via `wylde_shared::encryption` exactly like `concepts.json`
//! ([`super::store`]). Fail-soft: a missing/torn file ⇒ empty graph ⇒ routing
//! degrades to plain seed (no behaviour change).
//!
//! ## Validation (addendum §2.1, reusing what exists)
//!
//! * `NodeRef::Concept{id}` must resolve via [`super::store::get`];
//!   `NodeRef::Vocab{identifier}` via the anchor store membership — else
//!   `bad_request` (the `is_valid_identifier` discipline).
//! * Self-edges are rejected; symmetric kinds canonicalise orientation
//!   ([`Relation::normalized`]) so `A⊘B` and `B⊘A` are one record; a duplicate
//!   `(from,to,kind)` returns `already_exists` (the `store::create` idiom).

use std::path::PathBuf;

use serde_json::{json, Value};
use wylde_concept_routing::{NodeRef, Relation, RelationGraph, RelationKind};
use wylde_shared::ipc::{IpcError, Reply};

use crate::anchors::store as anchor_store;
use crate::registry::persistence::workspace_dir;

/// `<data_dir>/workspaces/<workspace_id>/concept_relations.json`.
pub fn relations_path(workspace_id: &str) -> PathBuf {
    workspace_dir(workspace_id).join("concept_relations.json")
}

/// Load the whole relation graph for a workspace. Fail-soft: empty on a
/// missing/torn file. Decrypts at rest (OI-14).
pub fn load(workspace_id: &str) -> RelationGraph {
    let Ok(raw) = wylde_shared::encryption::read_to_string_at_rest(&relations_path(workspace_id))
    else {
        return RelationGraph::empty();
    };
    serde_json::from_str(&raw).unwrap_or_else(|_| RelationGraph::empty())
}

/// Encrypt-at-rest (OI-14) + atomically replace `concept_relations.json`.
pub fn save(workspace_id: &str, graph: &RelationGraph) -> std::io::Result<()> {
    let body = serde_json::to_string_pretty(graph).unwrap();
    wylde_shared::encryption::write_at_rest(&relations_path(workspace_id), body.as_bytes())
}

// ── NodeRef resolution / parsing ─────────────────────────────────────────────

/// Does this node exist in the workspace? Concepts resolve via the concept
/// store; vocab via anchor-store membership.
fn node_exists(workspace_id: &str, node: &NodeRef) -> bool {
    match node {
        NodeRef::Concept { id } => super::store::get(workspace_id, id).is_some(),
        NodeRef::Vocab { identifier } => anchor_store::load(workspace_id)
            .iter()
            .any(|a| a.identifier == *identifier),
    }
}

/// Re-validate every relation after a concept recompute (Phase-B §4.2): flag an
/// edge `dangling` when an endpoint no longer resolves, and clear the flag when
/// it resolves again. **Never deletes** — the user's authored edge is retained
/// on disk (and surfaced in the tree for re-pointing) but excluded from routing
/// (`RelationGraph::adjacency`/`of_kind` skip dangling). Returns the count of
/// dangling edges after the sweep. No-op (saves nothing) on an empty graph or
/// when nothing changed.
pub fn sweep_dangling(workspace_id: &str) -> usize {
    let mut graph = load(workspace_id);
    if graph.relations.is_empty() {
        return 0;
    }
    let mut changed = false;
    let mut dangling = 0usize;
    for r in &mut graph.relations {
        let resolves = node_exists(workspace_id, &r.from) && node_exists(workspace_id, &r.to);
        let now_dangling = !resolves;
        if r.dangling != now_dangling {
            r.dangling = now_dangling;
            changed = true;
        }
        if now_dangling {
            dangling += 1;
        }
    }
    if changed {
        if let Err(e) = save(workspace_id, &graph) {
            tracing::warn!(
                "workspaces.concepts.relations: sweep_dangling save failed for {workspace_id}: {e}"
            );
        }
    }
    dangling
}

// Validation helpers return `Err(message)` — the error is always `bad_request`,
// so the caller wraps it (a small `String` Err keeps clippy's `result_large_err`
// quiet, vs returning the heavy `Reply` by value).

/// Parse a `NodeRef` from a payload key (the serde-tagged wire shape).
fn parse_node(payload: &Value, key: &str) -> Result<NodeRef, String> {
    let v = payload.get(key).ok_or_else(|| format!("{key} is required"))?;
    serde_json::from_value::<NodeRef>(v.clone()).map_err(|_| {
        format!("{key} must be {{node:\"concept\",id}} or {{node:\"vocab\",identifier}}")
    })
}

/// Parse the `kind` field.
fn parse_kind(payload: &Value) -> Result<RelationKind, String> {
    let v = payload.get("kind").ok_or("kind is required")?;
    serde_json::from_value::<RelationKind>(v.clone())
        .map_err(|_| "kind must be positive | negative | dependency".to_owned())
}

fn require_ws(payload: &Value) -> Result<String, String> {
    payload
        .get("workspace_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| "workspace_id is required".to_owned())
}

// ── Verb handlers ────────────────────────────────────────────────────────────

/// `workspaces.concepts.relations.graph` — the whole [`RelationGraph`] (tree
/// view + engine warm-load). Payload: `{workspace_id}`.
pub async fn handle_graph(payload: Value) -> Reply {
    let ws = match require_ws(&payload) {
        Ok(w) => w,
        Err(m) => return Reply::err_msg("bad_request", m),
    };
    let graph = load(&ws);
    let dangling_count = graph.relations.iter().filter(|r| r.dangling).count();
    Reply::ok(json!({
        "workspace_id": ws,
        "count": graph.relations.len(),
        "dangling_count": dangling_count,
        "relations": graph.relations,
    }))
}

/// `workspaces.concepts.relations.list` — edges touching `node` (both
/// directions), grouped by kind. Payload: `{workspace_id, node}`.
pub async fn handle_list(payload: Value) -> Reply {
    let ws = match require_ws(&payload) {
        Ok(w) => w,
        Err(m) => return Reply::err_msg("bad_request", m),
    };
    let node = match parse_node(&payload, "node") {
        Ok(n) => n,
        Err(m) => return Reply::err_msg("bad_request", m),
    };
    let graph = load(&ws);
    let touching: Vec<&Relation> = graph
        .relations
        .iter()
        .filter(|r| r.from == node || r.to == node)
        .collect();

    // Grouped for the authoring view: positive / negative are symmetric;
    // dependency splits into out (node depends-on X) and in (X depends-on node,
    // the backward blast radius).
    let positive: Vec<&&Relation> = touching
        .iter()
        .filter(|r| r.kind == RelationKind::Positive)
        .collect();
    let negative: Vec<&&Relation> = touching
        .iter()
        .filter(|r| r.kind == RelationKind::Negative)
        .collect();
    let dependency_out: Vec<&&Relation> = touching
        .iter()
        .filter(|r| r.kind == RelationKind::Dependency && r.from == node)
        .collect();
    let dependency_in: Vec<&&Relation> = touching
        .iter()
        .filter(|r| r.kind == RelationKind::Dependency && r.to == node)
        .collect();

    Reply::ok(json!({
        "workspace_id": ws,
        "node": node,
        "count": touching.len(),
        "relations": touching,
        "by_kind": {
            "positive": positive,
            "negative": negative,
            "dependency_out": dependency_out,
            "dependency_in": dependency_in,
        },
    }))
}

/// `workspaces.concepts.relations.add` — author one typed edge. Payload:
/// `{workspace_id, from, to, kind, note?}`. Idempotent: a duplicate
/// `(from,to,kind)` returns `already_exists` with the existing record.
pub async fn handle_add(payload: Value) -> Reply {
    let ws = match require_ws(&payload) {
        Ok(w) => w,
        Err(m) => return Reply::err_msg("bad_request", m),
    };
    let from = match parse_node(&payload, "from") {
        Ok(n) => n,
        Err(m) => return Reply::err_msg("bad_request", m),
    };
    let to = match parse_node(&payload, "to") {
        Ok(n) => n,
        Err(m) => return Reply::err_msg("bad_request", m),
    };
    let kind = match parse_kind(&payload) {
        Ok(k) => k,
        Err(m) => return Reply::err_msg("bad_request", m),
    };
    let note = payload
        .get("note")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    if from == to {
        return Reply::err_msg("bad_request", "a relation cannot connect a node to itself");
    }
    if !node_exists(&ws, &from) {
        return Reply::err_msg(
            "bad_request",
            format!("unknown `from` node: {}", from.label()),
        );
    }
    if !node_exists(&ws, &to) {
        return Reply::err_msg("bad_request", format!("unknown `to` node: {}", to.label()));
    }

    // Canonicalise (symmetric kinds collapse orientation), then stamp + dedupe.
    let mut rel = Relation::normalized(from, to, kind, note);
    rel.created_at = wylde_shared::anchor::epoch_now();

    let mut graph = load(&ws);
    if let Some(existing) = graph.relations.iter().find(|r| r.same_edge(&rel)) {
        return Reply::err(IpcError {
            code: "already_exists".into(),
            message: "this relation already exists".into(),
            details: Some(json!({ "relation": existing })),
        });
    }
    graph.relations.push(rel.clone());
    if let Err(e) = save(&ws, &graph) {
        return Reply::err_msg("io", format!("failed to persist relation: {e}"));
    }
    Reply::ok(json!({ "workspace_id": ws, "relation": rel }))
}

/// `workspaces.concepts.relations.remove` — delete one edge by
/// `(from,to,kind)`. Symmetric kinds match either orientation (the stored edge
/// is canonical). Payload: `{workspace_id, from, to, kind}`. Reply:
/// `{removed: bool}`.
pub async fn handle_remove(payload: Value) -> Reply {
    let ws = match require_ws(&payload) {
        Ok(w) => w,
        Err(m) => return Reply::err_msg("bad_request", m),
    };
    let from = match parse_node(&payload, "from") {
        Ok(n) => n,
        Err(m) => return Reply::err_msg("bad_request", m),
    };
    let to = match parse_node(&payload, "to") {
        Ok(n) => n,
        Err(m) => return Reply::err_msg("bad_request", m),
    };
    let kind = match parse_kind(&payload) {
        Ok(k) => k,
        Err(m) => return Reply::err_msg("bad_request", m),
    };
    // Canonicalise the query the same way add did, so a symmetric edge matches
    // regardless of the orientation the caller passed.
    let target = Relation::normalized(from, to, kind, None);

    let mut graph = load(&ws);
    let before = graph.relations.len();
    graph.relations.retain(|r| !r.same_edge(&target));
    let removed = graph.relations.len() != before;
    if removed {
        if let Err(e) = save(&ws, &graph) {
            return Reply::err_msg("io", format!("failed to persist removal: {e}"));
        }
    }
    Reply::ok(json!({ "workspace_id": ws, "removed": removed }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchors::store as anchors;
    use crate::concepts::concept::{Concept, ConceptSource};
    use crate::test_support::TestEnv;
    use wylde_shared::anchor::{Anchor, AnchorKind, AnchorScope, AnchorTarget};

    fn vocab(ws: &str, identifier: &str) -> Anchor {
        Anchor::new(
            identifier,
            AnchorKind::Concept,
            AnchorTarget::Concept {
                text: identifier.into(),
            },
            AnchorScope::Workspace {
                workspace_id: ws.into(),
            },
            format!("the {identifier}"),
        )
    }

    fn seed_nodes(ws: &str) {
        // Two concepts + two vocab anchors to relate.
        super::super::store::save(
            ws,
            &[
                Concept::new(
                    "nextcloud",
                    "Nextcloud",
                    "self-hosted sync",
                    ConceptSource::Manual,
                ),
                Concept::new("wylde", "Wylde", "the assistant", ConceptSource::Manual),
            ],
        )
        .unwrap();
        anchors::save(ws, &[vocab(ws, "ddns"), vocab(ws, "vpn")]).unwrap();
    }

    #[tokio::test]
    async fn add_list_remove_round_trip() {
        let _env = TestEnv::new();
        let ws = "rel-rt-000000";
        seed_nodes(ws);

        // Add: Nextcloud depends-on {{ddns}}.
        let add = handle_add(json!({
            "workspace_id": ws,
            "from": {"node":"concept","id":"nextcloud"},
            "to": {"node":"vocab","identifier":"ddns"},
            "kind": "dependency",
            "note": "keeps the home IP current",
        }))
        .await;
        assert!(add.ok, "add failed: {:?}", add.error);

        // Persisted + reloads.
        let g = load(ws);
        assert_eq!(g.relations.len(), 1);
        assert_eq!(g.relations[0].kind, RelationKind::Dependency);

        // List touching the concept groups it as a dependency_out edge.
        let list = handle_list(json!({
            "workspace_id": ws,
            "node": {"node":"concept","id":"nextcloud"},
        }))
        .await;
        assert!(list.ok);
        assert_eq!(
            list.data["by_kind"]["dependency_out"]
                .as_array()
                .unwrap()
                .len(),
            1
        );

        // Remove it.
        let rm = handle_remove(json!({
            "workspace_id": ws,
            "from": {"node":"concept","id":"nextcloud"},
            "to": {"node":"vocab","identifier":"ddns"},
            "kind": "dependency",
        }))
        .await;
        assert!(rm.ok && rm.data["removed"] == json!(true));
        assert!(load(ws).is_empty());
    }

    #[tokio::test]
    async fn add_rejects_unknown_nodes_and_self_edge() {
        let _env = TestEnv::new();
        let ws = "rel-bad-000000";
        seed_nodes(ws);

        // Unknown concept id.
        let r = handle_add(json!({
            "workspace_id": ws,
            "from": {"node":"concept","id":"ghost"},
            "to": {"node":"vocab","identifier":"ddns"},
            "kind": "dependency",
        }))
        .await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "bad_request");

        // Self-edge.
        let r = handle_add(json!({
            "workspace_id": ws,
            "from": {"node":"concept","id":"wylde"},
            "to": {"node":"concept","id":"wylde"},
            "kind": "negative",
        }))
        .await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn symmetric_edge_dedupes_across_orientation() {
        let _env = TestEnv::new();
        let ws = "rel-sym-000000";
        seed_nodes(ws);

        // Nextcloud ⊘ Wylde …
        let a = handle_add(json!({
            "workspace_id": ws,
            "from": {"node":"concept","id":"nextcloud"},
            "to": {"node":"concept","id":"wylde"},
            "kind": "negative",
        }))
        .await;
        assert!(a.ok);

        // … and the reverse orientation is the SAME edge ⇒ already_exists.
        let b = handle_add(json!({
            "workspace_id": ws,
            "from": {"node":"concept","id":"wylde"},
            "to": {"node":"concept","id":"nextcloud"},
            "kind": "negative",
        }))
        .await;
        assert!(!b.ok);
        assert_eq!(b.error.unwrap().code, "already_exists");
        assert_eq!(load(ws).relations.len(), 1, "stored once");

        // Remove via the reverse orientation still matches.
        let rm = handle_remove(json!({
            "workspace_id": ws,
            "from": {"node":"concept","id":"wylde"},
            "to": {"node":"concept","id":"nextcloud"},
            "kind": "negative",
        }))
        .await;
        assert!(rm.ok && rm.data["removed"] == json!(true));
    }

    #[tokio::test]
    async fn missing_file_is_empty_graph() {
        let _env = TestEnv::new();
        assert!(load("rel-none-00000").is_empty());
    }

    #[tokio::test]
    async fn sweep_flags_vanished_concept_never_deletes_and_clears_on_return() {
        let _env = TestEnv::new();
        let ws = "rel-dangle-0000";
        seed_nodes(ws);
        // Author an edge between the two concepts.
        let add = handle_add(json!({
            "workspace_id": ws,
            "from": {"node":"concept","id":"nextcloud"},
            "to": {"node":"concept","id":"wylde"},
            "kind": "negative",
        }))
        .await;
        assert!(add.ok);
        assert_eq!(sweep_dangling(ws), 0, "both endpoints resolve initially");

        // A recompute drops the `wylde` concept (id no longer in the store).
        super::super::store::delete(ws, "wylde").unwrap();
        let dangling = sweep_dangling(ws);
        assert_eq!(dangling, 1, "edge to a vanished concept is flagged");
        // The edge is RETAINED on disk, just flagged — never deleted.
        let g = load(ws);
        assert_eq!(g.relations.len(), 1, "edge kept, not deleted");
        assert!(g.relations[0].dangling, "flag set");
        // …and excluded from routing.
        assert!(g.adjacency().is_empty(), "dangling edge absent from routing adjacency");

        // The concept returns on a later build → the flag clears (re-validate).
        super::super::store::create(
            ws,
            Concept::new("wylde", "Wylde", "the assistant", ConceptSource::Manual),
        )
        .unwrap();
        assert_eq!(sweep_dangling(ws), 0, "flag cleared when the endpoint returns");
        assert!(!load(ws).relations[0].dangling);
    }
}
