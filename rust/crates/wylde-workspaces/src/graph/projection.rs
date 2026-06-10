//! Turn raw Neo4j rows ([`super::query::GraphRows`]) into the serializable
//! [`WorkspaceGraph`] the `workspaces.graph` verb returns (Slice B).
//!
//! ## What is real vs. defaulted in v1
//!
//! The persisted graph stores only an entity's **name** (its key) plus, via
//! `MENTIONED_IN`, the **file** + **language** of a chunk it appears in. It
//! does **not** store a node's kind, definition line, screen position, or
//! render style. So this projection returns:
//!
//!   * **Real, from Neo4j:** `id`/`name` (the entity key), `file` (a chunk
//!     path the entity is mentioned in), and every `Edge` (`src`, `dst`,
//!     `rel_type`).
//!   * **Best-effort heuristic:** `kind` — inferred from each entity's role
//!     in the edge set (import endpoint → `Module`, inheritance endpoint →
//!     `Class`, otherwise `Function`). The graph doesn't record kind; a
//!     future ingest enrichment (or Slice F-data's symbol index) can replace
//!     this with a stored value.
//!   * **Defaulted, computed in Phase 3:** `line` (0), `position` (origin;
//!     `z` is always 0 per Plan v2 §10), `style` (empty hints). The real
//!     layout / styling / full clustering live in the graph panel
//!     (C-scaffold … C-cluster).
//!
//! Clusters are a simple v1 derivation: one cluster per **file parent
//! directory**, grouping the nodes whose representative file lives there.
//! Nodes without a file (synthesised external edge targets) are left
//! unclustered. Hierarchical / centrality-based clustering is C-cluster.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::query::GraphRows;

/// Default edge weight — the graph stores no weight, so every edge is 1.0
/// until a future slice attaches call-frequency / centrality weights.
pub const DEFAULT_EDGE_WEIGHT: f32 = 1.0;

/// Default cluster zoom threshold (a Phase 3 rendering hint). The real
/// per-cluster thresholds are computed by C-cluster.
pub const DEFAULT_ZOOM_THRESHOLD: f32 = 1.0;

/// What a graph node is. The graph doesn't persist kind; [`classify`] infers
/// it from edge roles in v1. `Constant`/`Enum`/`Anchor` are part of the wire
/// contract for later slices (ingest enrichment, the anchor overlay) but are
/// not produced by the v1 heuristic.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeKind {
    Function,
    Class,
    Trait,
    Module,
    Constant,
    Enum,
    Anchor,
}

/// The Entity→Entity relation vocabulary, wire-serialised in SCREAMING form
/// to match Neo4j's `type(r)` strings and [`super::schema`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "SCREAMING_SNAKE_CASE")]
pub enum RelType {
    Calls,
    Imports,
    Inherits,
    Configures,
    Exposes,
}

impl RelType {
    /// Parse a raw `type(r)` string (e.g. `"CALLS"`) into a [`RelType`].
    /// Case-insensitive; `None` for anything outside the vocabulary
    /// (notably `MENTIONED_IN`, which the read query never selects).
    pub fn from_wire(s: &str) -> Option<Self> {
        match s.trim().to_ascii_uppercase().as_str() {
            "CALLS" => Some(RelType::Calls),
            "IMPORTS" => Some(RelType::Imports),
            "INHERITS" => Some(RelType::Inherits),
            "CONFIGURES" => Some(RelType::Configures),
            "EXPOSES" => Some(RelType::Exposes),
            _ => None,
        }
    }
}

/// A 3D position. `z` is always 0 in v1 (Plan v2 §10) — the field exists so
/// the v2 3D upgrade is a renderer swap, not a struct rewrite.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Position {
    pub x: f32,
    pub y: f32,
    pub z: f32,
}

/// Per-node rendering hints. Empty in v1 — the graph panel (Phase 3) computes
/// real colors/sizes from `kind` + theme. Present so the wire shape is stable.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NodeStyle {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub radius: Option<f32>,
}

/// A graph node. See the module docs for what is real vs. defaulted in v1.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub kind: NodeKind,
    pub name: String,
    pub file: PathBuf,
    pub line: u32,
    pub position: Position,
    pub style: NodeStyle,
}

/// A typed graph edge between two node ids.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Edge {
    pub src: String,
    pub dst: String,
    pub rel_type: RelType,
    pub weight: f32,
}

/// A collapsible group of nodes. v1: one per file parent directory.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Cluster {
    pub id: String,
    pub member_ids: Vec<String>,
    pub parent_breadcrumb: Vec<String>,
    pub zoom_threshold: f32,
}

/// The top-level graph state the `workspaces.graph` verb returns.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceGraph {
    pub nodes: Vec<Node>,
    pub edges: Vec<Edge>,
    pub clusters: Vec<Cluster>,
}

/// Project decoded Neo4j rows into a [`WorkspaceGraph`]. Pure + deterministic
/// (everything is sorted) so the verb's output is stable across runs and the
/// unit tests need no live database.
pub fn project(rows: GraphRows) -> WorkspaceGraph {
    // 1. Parse + dedup edges; drop any row outside the relation vocabulary.
    let mut edges: Vec<Edge> = Vec::with_capacity(rows.edges.len());
    let mut edge_keys: BTreeSet<(String, String, String)> = BTreeSet::new();
    for e in &rows.edges {
        let Some(rel) = RelType::from_wire(&e.rel) else {
            continue;
        };
        if e.src.is_empty() || e.dst.is_empty() {
            continue;
        }
        let key = (e.src.clone(), e.rel.to_ascii_uppercase(), e.dst.clone());
        if edge_keys.insert(key) {
            edges.push(Edge {
                src: e.src.clone(),
                dst: e.dst.clone(),
                rel_type: rel,
                weight: DEFAULT_EDGE_WEIGHT,
            });
        }
    }
    edges.sort_by(|a, b| {
        (a.src.as_str(), wire_of(a.rel_type), a.dst.as_str()).cmp(&(
            b.src.as_str(),
            wire_of(b.rel_type),
            b.dst.as_str(),
        ))
    });

    // 2. Edge-role sets for the kind heuristic.
    let mut import_endpoints: BTreeSet<&str> = BTreeSet::new();
    let mut inherit_endpoints: BTreeSet<&str> = BTreeSet::new();
    for e in &edges {
        match e.rel_type {
            RelType::Imports => {
                import_endpoints.insert(e.src.as_str());
                import_endpoints.insert(e.dst.as_str());
            }
            RelType::Inherits => {
                inherit_endpoints.insert(e.src.as_str());
                inherit_endpoints.insert(e.dst.as_str());
            }
            _ => {}
        }
    }

    // 3. Node set = workspace entities ∪ every edge endpoint. Workspace
    //    entities carry a file/language; external edge targets get empty
    //    ones so every edge still resolves to a node.
    let mut files: BTreeMap<String, String> = BTreeMap::new();
    for n in &rows.nodes {
        if n.name.is_empty() {
            continue;
        }
        // First mention wins (rows are already `min`-deterministic).
        files
            .entry(n.name.clone())
            .or_insert_with(|| n.file.clone());
    }
    let mut names: BTreeSet<String> = files.keys().cloned().collect();
    for e in &edges {
        names.insert(e.src.clone());
        names.insert(e.dst.clone());
    }

    let nodes: Vec<Node> = names
        .into_iter()
        .map(|name| {
            let file = files.get(&name).cloned().unwrap_or_default();
            Node {
                kind: classify(&name, &import_endpoints, &inherit_endpoints),
                id: name.clone(),
                name,
                file: PathBuf::from(file),
                line: 0,
                position: Position::default(),
                style: NodeStyle::default(),
            }
        })
        .collect();

    let clusters = cluster_by_dir(&nodes);

    WorkspaceGraph {
        nodes,
        edges,
        clusters,
    }
}

/// Best-effort v1 kind from edge roles (see module docs). Priority:
/// import endpoint → `Module`, else inheritance endpoint → `Class`, else
/// `Function`.
fn classify(name: &str, imports: &BTreeSet<&str>, inherits: &BTreeSet<&str>) -> NodeKind {
    if imports.contains(name) {
        NodeKind::Module
    } else if inherits.contains(name) {
        NodeKind::Class
    } else {
        NodeKind::Function
    }
}

/// One cluster per distinct file parent directory. Nodes without a usable
/// parent directory (no file, or a bare filename) are left unclustered.
fn cluster_by_dir(nodes: &[Node]) -> Vec<Cluster> {
    let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for n in nodes {
        if let Some(dir) = parent_dir_key(&n.file) {
            groups.entry(dir).or_default().push(n.id.clone());
        }
    }
    groups
        .into_iter()
        .map(|(dir, mut members)| {
            members.sort();
            members.dedup();
            Cluster {
                parent_breadcrumb: breadcrumb(&dir),
                id: dir,
                member_ids: members,
                zoom_threshold: DEFAULT_ZOOM_THRESHOLD,
            }
        })
        .collect()
}

/// The parent directory of `file` as a string key, or `None` when `file` is
/// empty or has no directory component (a bare filename).
fn parent_dir_key(file: &Path) -> Option<String> {
    if file.as_os_str().is_empty() {
        return None;
    }
    let parent = file.parent()?;
    if parent.as_os_str().is_empty() {
        return None;
    }
    Some(parent.to_string_lossy().into_owned())
}

/// The normal (non-prefix, non-root) path components of `dir`, as a
/// breadcrumb. A path-derived v1 approximation; the workspace-relative,
/// cluster-hierarchy breadcrumb is C-cluster's job.
fn breadcrumb(dir: &str) -> Vec<String> {
    Path::new(dir)
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect()
}

/// The wire string for a [`RelType`] (for deterministic edge sorting).
fn wire_of(r: RelType) -> &'static str {
    match r {
        RelType::Calls => "CALLS",
        RelType::Imports => "IMPORTS",
        RelType::Inherits => "INHERITS",
        RelType::Configures => "CONFIGURES",
        RelType::Exposes => "EXPOSES",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::query::{EdgeRow, NodeRow};

    fn node_row(name: &str, file: &str, lang: &str) -> NodeRow {
        NodeRow {
            name: name.to_owned(),
            file: file.to_owned(),
            language: lang.to_owned(),
        }
    }
    fn edge_row(src: &str, dst: &str, rel: &str) -> EdgeRow {
        EdgeRow {
            src: src.to_owned(),
            dst: dst.to_owned(),
            rel: rel.to_owned(),
        }
    }

    fn find<'a>(g: &'a WorkspaceGraph, id: &str) -> &'a Node {
        g.nodes
            .iter()
            .find(|n| n.id == id)
            .unwrap_or_else(|| panic!("node {id} missing from {:?}", g.nodes))
    }

    /// A small fixture mirroring a real Rust file's extraction: a module
    /// identity, a free fn that calls another, a class inheriting a base, an
    /// import to stdlib.
    fn sample_rows() -> GraphRows {
        GraphRows {
            nodes: vec![
                node_row("widget", "C:/ws/src/widget.rs", "rust"),
                node_row("alpha", "C:/ws/src/widget.rs", "rust"),
                node_row("beta", "C:/ws/src/widget.rs", "rust"),
                node_row("Widget", "C:/ws/src/widget.rs", "rust"),
            ],
            edges: vec![
                edge_row("alpha", "beta", "CALLS"),
                edge_row("widget", "std::collections", "IMPORTS"),
                edge_row("Widget", "Render", "INHERITS"),
            ],
        }
    }

    #[test]
    fn projects_nodes_edges_and_synthesises_external_targets() {
        let g = project(sample_rows());

        // 4 workspace entities + 2 external edge targets (std::collections,
        // Render) that were never mentioned in a chunk.
        let ids: BTreeSet<&str> = g.nodes.iter().map(|n| n.id.as_str()).collect();
        assert!(ids.contains("alpha") && ids.contains("beta") && ids.contains("Widget"));
        assert!(
            ids.contains("std::collections") && ids.contains("Render"),
            "external edge targets become nodes: {ids:?}"
        );
        assert_eq!(g.nodes.len(), 6);
        assert_eq!(g.edges.len(), 3);
    }

    #[test]
    fn edges_carry_parsed_rel_type_and_default_weight() {
        let g = project(sample_rows());
        let call = g
            .edges
            .iter()
            .find(|e| e.src == "alpha" && e.dst == "beta")
            .unwrap();
        assert_eq!(call.rel_type, RelType::Calls);
        assert_eq!(call.weight, DEFAULT_EDGE_WEIGHT);
        assert!(g.edges.iter().any(|e| e.rel_type == RelType::Imports));
        assert!(g.edges.iter().any(|e| e.rel_type == RelType::Inherits));
    }

    #[test]
    fn kind_heuristic_classifies_by_edge_role() {
        let g = project(sample_rows());
        // import endpoints → Module
        assert_eq!(find(&g, "widget").kind, NodeKind::Module);
        assert_eq!(find(&g, "std::collections").kind, NodeKind::Module);
        // inheritance endpoints → Class
        assert_eq!(find(&g, "Widget").kind, NodeKind::Class);
        assert_eq!(find(&g, "Render").kind, NodeKind::Class);
        // plain call participants → Function
        assert_eq!(find(&g, "alpha").kind, NodeKind::Function);
        assert_eq!(find(&g, "beta").kind, NodeKind::Function);
    }

    #[test]
    fn real_nodes_carry_file_externals_do_not() {
        let g = project(sample_rows());
        assert_eq!(find(&g, "alpha").file, PathBuf::from("C:/ws/src/widget.rs"));
        assert_eq!(find(&g, "Render").file, PathBuf::new());
    }

    #[test]
    fn position_is_origin_and_z_is_zero() {
        let g = project(sample_rows());
        for n in &g.nodes {
            assert_eq!(n.position, Position::default());
            assert_eq!(n.position.z, 0.0, "z is forced to 0 in v1");
            assert_eq!(n.line, 0);
        }
    }

    #[test]
    fn clusters_group_workspace_nodes_by_parent_dir() {
        let g = project(sample_rows());
        // All four workspace entities live in C:/ws/src → one cluster.
        // External targets have no file → unclustered.
        assert_eq!(g.clusters.len(), 1);
        let c = &g.clusters[0];
        assert!(c.member_ids.contains(&"alpha".to_owned()));
        assert!(c.member_ids.contains(&"Widget".to_owned()));
        assert!(!c.member_ids.contains(&"Render".to_owned()));
        assert!(
            c.parent_breadcrumb.ends_with(&["src".to_owned()]),
            "breadcrumb from path components: {:?}",
            c.parent_breadcrumb
        );
        assert_eq!(c.zoom_threshold, DEFAULT_ZOOM_THRESHOLD);
    }

    #[test]
    fn output_is_deterministic_regardless_of_row_order() {
        let mut rows = sample_rows();
        let a = project(rows.clone());
        rows.nodes.reverse();
        rows.edges.reverse();
        let b = project(rows);
        assert_eq!(a, b, "projection must be order-independent");
    }

    #[test]
    fn duplicate_edges_and_unknown_rel_types_are_dropped() {
        let rows = GraphRows {
            nodes: vec![node_row("a", "x.rs", "rust")],
            edges: vec![
                edge_row("a", "b", "CALLS"),
                edge_row("a", "b", "CALLS"),        // duplicate
                edge_row("a", "c", "MENTIONED_IN"), // not in vocabulary
                edge_row("", "d", "CALLS"),         // empty endpoint
            ],
        };
        let g = project(rows);
        assert_eq!(g.edges.len(), 1, "dup + non-vocab + empty all dropped");
        assert_eq!(g.edges[0].dst, "b");
    }

    #[test]
    fn empty_rows_yield_empty_graph() {
        let g = project(GraphRows::default());
        assert!(g.nodes.is_empty() && g.edges.is_empty() && g.clusters.is_empty());
    }

    #[test]
    fn rel_type_round_trips_through_wire() {
        for (s, r) in [
            ("CALLS", RelType::Calls),
            ("imports", RelType::Imports),
            ("INHERITS", RelType::Inherits),
            ("Configures", RelType::Configures),
            ("EXPOSES", RelType::Exposes),
        ] {
            assert_eq!(RelType::from_wire(s), Some(r));
            // serde serialises to the SCREAMING wire form.
            assert_eq!(
                serde_json::to_value(r).unwrap(),
                serde_json::Value::String(wire_of(r).to_owned())
            );
        }
        assert_eq!(RelType::from_wire("MENTIONED_IN"), None);
    }

    #[test]
    fn workspace_graph_serialises_to_expected_json_shape() {
        let g = project(sample_rows());
        let v = serde_json::to_value(&g).unwrap();
        assert!(v.get("nodes").is_some());
        assert!(v.get("edges").is_some());
        assert!(v.get("clusters").is_some());
        // A node serialises with the documented field set; kind is a string.
        let node = &v["nodes"][0];
        for field in ["id", "kind", "name", "file", "line", "position"] {
            assert!(node.get(field).is_some(), "node missing {field}");
        }
        assert!(node["kind"].is_string());
        assert!(v["edges"][0]["rel_type"].is_string());
    }
}
