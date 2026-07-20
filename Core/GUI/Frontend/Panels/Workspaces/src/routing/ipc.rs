//! Relations editor → pipe calls (concept-routing **R1.5c**, relation-model
//! addendum `outputs/concept-routing-relation-model.md` §2): the per-workspace
//! typed-relation store on `wylde-workspaces`
//! (`workspaces.concepts.relations.{graph,list,add,remove}`, shipped R1.5a).
//!
//! `NodeRefView` / `RelationView` mirror the crate's wire shapes locally — the
//! same GUI-decoupling convention as [`super::super::vocabulary::ipc`]: the GUI
//! crate doesn't link the service crate, and serde defaults keep older records
//! loading. The node *universe* (the pickers' candidates) reuses the Concepts
//! and Vocabulary sub-tabs' own loaders rather than a new verb.
//!
//! All authoring is behaviour-safe: writing relations only shapes routing when
//! the master toggle is ON (default OFF), and nothing is injected until R2.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::vocabulary::concepts_ipc;
use crate::vocabulary::ipc as vocab_ipc;

const SVC_WORKSPACES: &str = "wylde-workspaces";

/// The GUI mirror of `wylde_concept_routing::NodeRef` — a node in the relation
/// graph is either a concept (keyed by store id) or a vocabulary anchor (keyed
/// by `{{identifier}}`). Internally-tagged wire shape, byte-for-byte the verb's
/// `{"node":"concept","id":…}` | `{"node":"vocab","identifier":…}`.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(tag = "node", rename_all = "snake_case")]
pub enum NodeRefView {
    Concept { id: String },
    Vocab { identifier: String },
}

impl NodeRefView {
    pub fn concept(id: impl Into<String>) -> Self {
        NodeRefView::Concept { id: id.into() }
    }
    pub fn vocab(identifier: impl Into<String>) -> Self {
        NodeRefView::Vocab {
            identifier: identifier.into(),
        }
    }

    /// The verb payload shape (the tagged JSON the handler parses back into a
    /// `NodeRef`).
    pub fn to_payload(&self) -> Value {
        match self {
            NodeRefView::Concept { id } => json!({ "node": "concept", "id": id }),
            NodeRefView::Vocab { identifier } => {
                json!({ "node": "vocab", "identifier": identifier })
            }
        }
    }

    /// The raw key (concept store id, or the bare vocab identifier).
    pub fn key(&self) -> &str {
        match self {
            NodeRefView::Concept { id } => id,
            NodeRefView::Vocab { identifier } => identifier,
        }
    }

    pub fn is_concept(&self) -> bool {
        matches!(self, NodeRefView::Concept { .. })
    }
}

/// The GUI mirror of `wylde_concept_routing::RelationKind`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RelationKindView {
    Positive,
    Negative,
    Dependency,
}

impl RelationKindView {
    /// The wire string the verb expects in its `kind` field.
    pub fn as_wire(self) -> &'static str {
        match self {
            RelationKindView::Positive => "positive",
            RelationKindView::Negative => "negative",
            RelationKindView::Dependency => "dependency",
        }
    }
}

/// The GUI mirror of one stored `Relation` (the `relations[]` / `relation` wire
/// shape the verbs return).
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct RelationView {
    pub from: NodeRefView,
    pub to: NodeRefView,
    pub kind: RelationKindView,
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub created_at: f64,
    /// Whether an endpoint no longer resolves, so the edge is retained on disk
    /// but **excluded from routing** (#137).
    ///
    /// The backend has always sent this — `sweep_dangling` runs at the tail of
    /// every concept build and the wire shape carries the flag — but this
    /// struct did not deserialise it, so the Relations editor rendered a
    /// silently inert edge as if it were live. The Hierarchy sub-tab mirrors
    /// and badges the same flag; the two views disagreed about the same data.
    ///
    /// `#[serde(default)]` so an older backend (no field) reads as "live",
    /// which is the safe direction: it under-warns rather than falsely
    /// flagging every edge.
    #[serde(default)]
    pub dangling: bool,
}

/// One pickable node + its human label (the picker candidate / list row model).
/// The label resolves a concept's `id` to its `label`; a vocab node keeps its
/// `{{identifier}}` slug.
#[derive(Clone, Debug, PartialEq)]
pub struct NodeItem {
    pub node: NodeRefView,
    pub label: String,
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

fn parse_relations(v: &Value) -> Vec<RelationView> {
    v.get("relations")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|r| serde_json::from_value::<RelationView>(r.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// `workspaces.concepts.relations.graph` — the whole relation set (the overview
/// + the seam R3's typed-edge tree reads from).
pub async fn load_graph(ws: &str) -> Result<Vec<RelationView>, String> {
    let v = workspaces_call(
        "workspaces.concepts.relations.graph",
        json!({ "workspace_id": ws }),
    )
    .await?;
    Ok(parse_relations(&v))
}

/// `workspaces.concepts.relations.list` — every edge touching `node` (both
/// directions). The view does its own per-group bucketing, so we just return
/// the flat touching set.
pub async fn list_for_node(ws: &str, node: &NodeRefView) -> Result<Vec<RelationView>, String> {
    let v = workspaces_call(
        "workspaces.concepts.relations.list",
        json!({ "workspace_id": ws, "node": node.to_payload() }),
    )
    .await?;
    Ok(parse_relations(&v))
}

/// `workspaces.concepts.relations.add` — author one typed edge. The verb
/// validates (self-edge / unknown node → `bad_request`; duplicate →
/// `already_exists`); the error string flows back for inline feedback.
pub async fn add_relation(
    ws: &str,
    from: &NodeRefView,
    to: &NodeRefView,
    kind: RelationKindView,
    note: Option<&str>,
) -> Result<Value, String> {
    let mut payload = json!({
        "workspace_id": ws,
        "from": from.to_payload(),
        "to": to.to_payload(),
        "kind": kind.as_wire(),
    });
    if let Some(n) = note.map(str::trim).filter(|s| !s.is_empty()) {
        payload["note"] = json!(n);
    }
    workspaces_call("workspaces.concepts.relations.add", payload).await
}

/// `workspaces.concepts.relations.remove` — delete one edge by `(from,to,kind)`.
/// Symmetric kinds match either orientation (the store is canonical).
pub async fn remove_relation(
    ws: &str,
    from: &NodeRefView,
    to: &NodeRefView,
    kind: RelationKindView,
) -> Result<Value, String> {
    workspaces_call(
        "workspaces.concepts.relations.remove",
        json!({
            "workspace_id": ws,
            "from": from.to_payload(),
            "to": to.to_payload(),
            "kind": kind.as_wire(),
        }),
    )
    .await
}

/// Load the node *universe* for the pickers + label resolution: every concept
/// (via the hybrid-search verb with an empty query = the full set) plus every
/// workspace vocabulary anchor. Concepts sort before vocab; each sub-list is
/// label-ordered. Best-effort — a failure on one side still returns the other.
pub async fn load_node_universe(ws: &str) -> Vec<NodeItem> {
    let mut items: Vec<NodeItem> = Vec::new();
    if let Ok(concepts) = concepts_ipc::search_concepts(ws, "", 500).await {
        for s in concepts {
            items.push(NodeItem {
                node: NodeRefView::concept(s.concept.id.clone()),
                label: if s.concept.label.is_empty() {
                    s.concept.id
                } else {
                    s.concept.label
                },
            });
        }
    }
    if let Ok(anchors) = vocab_ipc::list_workspace_anchors(ws).await {
        for a in anchors {
            items.push(NodeItem {
                label: format!("{{{{{}}}}}", a.identifier),
                node: NodeRefView::vocab(a.identifier),
            });
        }
    }
    items
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_ref_round_trips_the_tagged_wire_shape() {
        let c = NodeRefView::concept("dir:src/graph");
        assert_eq!(
            c.to_payload(),
            json!({ "node": "concept", "id": "dir:src/graph" })
        );
        let back: NodeRefView = serde_json::from_value(c.to_payload()).unwrap();
        assert_eq!(back, c);

        let v = NodeRefView::vocab("nextcloud");
        assert_eq!(
            v.to_payload(),
            json!({ "node": "vocab", "identifier": "nextcloud" })
        );
        let back: NodeRefView = serde_json::from_value(v.to_payload()).unwrap();
        assert_eq!(back, v);
        assert_eq!(back.key(), "nextcloud");
        assert!(c.is_concept() && !v.is_concept());
    }

    /// #137 — the backend flags an edge whose endpoint no longer resolves as
    /// `dangling` (retained on disk, excluded from routing). This struct used
    /// to drop the field entirely, so the Relations editor rendered a silently
    /// inert edge exactly like a live one, while the Hierarchy sub-tab badged
    /// the same flag correctly. Two views, same data, opposite stories.
    #[test]
    fn relation_carries_the_dangling_flag_the_backend_sends() {
        let v = json!({
            "from": { "node": "concept", "id": "sem:0007" },
            "to": { "node": "concept", "id": "sem:0009" },
            "kind": "positive",
            "dangling": true
        });
        let r: RelationView = serde_json::from_value(v).unwrap();
        assert!(
            r.dangling,
            "the dangling flag must survive deserialisation — the user cannot \
             re-point an edge they cannot see is broken"
        );

        // A live edge (and an older backend that omits the key) reads as live.
        let live: RelationView = serde_json::from_value(json!({
            "from": { "node": "concept", "id": "sem:0007" },
            "to": { "node": "concept", "id": "sem:0009" },
            "kind": "positive"
        }))
        .unwrap();
        assert!(!live.dangling, "absent flag defaults to live, not broken");
    }

    /// The flag has to reach the row model too — `group_edges` builds
    /// `GroupEdge` from `RelationView`, and dropping it there would hide the
    /// badge just as effectively as dropping it at the wire.
    #[test]
    fn grouping_preserves_the_dangling_flag_through_to_the_row() {
        use crate::routing::reducer;
        let focus = NodeRefView::concept("sem:0007");
        let touching = vec![
            RelationView {
                from: NodeRefView::concept("sem:0007"),
                to: NodeRefView::concept("sem:0009"),
                kind: RelationKindView::Positive,
                note: None,
                created_at: 0.0,
                dangling: true,
            },
            RelationView {
                from: NodeRefView::concept("sem:0007"),
                to: NodeRefView::concept("sem:0010"),
                kind: RelationKindView::Positive,
                note: None,
                created_at: 0.0,
                dangling: false,
            },
        ];
        let groups = reducer::group_edges(&focus, &touching);
        let edges: Vec<_> = groups.into_iter().flat_map(|(_, e)| e).collect();
        assert_eq!(edges.len(), 2);
        assert_eq!(
            edges.iter().filter(|e| e.dangling).count(),
            1,
            "exactly the dangling edge is flagged in the row model"
        );
    }

    #[test]
    fn relation_parses_with_optional_note() {
        let v = json!({
            "from": { "node": "concept", "id": "nextcloud" },
            "to": { "node": "vocab", "identifier": "ddns" },
            "kind": "dependency",
            "note": "keeps the home IP current",
            "created_at": 12.0
        });
        let r: RelationView = serde_json::from_value(v).unwrap();
        assert_eq!(r.from, NodeRefView::concept("nextcloud"));
        assert_eq!(r.to, NodeRefView::vocab("ddns"));
        assert_eq!(r.kind, RelationKindView::Dependency);
        assert_eq!(r.note.as_deref(), Some("keeps the home IP current"));

        // A note-less edge still parses (the store omits the key).
        let bare: RelationView = serde_json::from_value(json!({
            "from": { "node": "vocab", "identifier": "a" },
            "to": { "node": "vocab", "identifier": "b" },
            "kind": "negative"
        }))
        .unwrap();
        assert_eq!(bare.note, None);
        assert_eq!(bare.kind, RelationKindView::Negative);
    }

    #[test]
    fn parse_relations_reads_the_array_envelope() {
        let v = json!({
            "count": 2,
            "relations": [
                { "from": {"node":"concept","id":"a"}, "to": {"node":"concept","id":"b"}, "kind": "positive" },
                { "from": {"node":"concept","id":"a"}, "to": {"node":"vocab","identifier":"x"}, "kind": "dependency" }
            ]
        });
        let rels = parse_relations(&v);
        assert_eq!(rels.len(), 2);
        assert_eq!(rels[1].to, NodeRefView::vocab("x"));
    }

    #[test]
    fn kind_wire_strings_match_the_verb() {
        assert_eq!(RelationKindView::Positive.as_wire(), "positive");
        assert_eq!(RelationKindView::Negative.as_wire(), "negative");
        assert_eq!(RelationKindView::Dependency.as_wire(), "dependency");
    }
}
