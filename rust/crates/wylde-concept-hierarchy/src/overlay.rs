//! The **overlay** -- the only net-new persisted data the hierarchy adds, plus
//! the pure fold that layers it onto a projected [`HierGraph`]
//! (definitional-hierarchy plan SS2.2, slice H1).
//!
//! Pure serde + pure functions, no I/O -- the same discipline as
//! [`crate::model`] and `wylde_concept_routing::relations`. The on-disk store
//! ([`HierarchyOverlay`]) and the id allocator ([`HierarchyIdentity`]) are
//! *types* here; the encrypted/atomic/fail-soft persistence + the verbs that
//! mutate them live in the deletable `wylde-workspaces` bridge (H1), exactly as
//! the relation types live here while `relations_bridge.rs` does the I/O.
//!
//! ## What the overlay holds (and what it deliberately does not)
//!
//! The projection ([`crate::build_view`]) already draws the whole DAG the
//! existing stores can express (`parent_concepts` / `parent_anchor` /
//! `described_by`). The overlay holds **only what those stores cannot**:
//!
//! * an **authored / overriding definition** for a node (the top rung of the
//!   priority ladder, plan SS1) and/or a label override;
//! * a brand-new **authored node** the source stores have no record of (a
//!   `node:<n>` id minted by [`HierarchyIdentity`]);
//! * an **authored containment edge** (`parent -> child`) the source stores
//!   can't draw (e.g. "`workflows` is under `N8N`" when both are bare vocab
//!   anchors with no `parent_anchor`);
//! * a **merge** declaring that two projected nodes are in fact one (OQ-2: the
//!   explicit user action that collapses a concept and its naming vocab term).
//!
//! ## Dangling retention (lifted verbatim from `Relation.dangling`)
//!
//! An overlay edge / merge whose endpoint vanishes on a concept recompute is
//! **retained on disk, flagged `dangling`, surfaced for re-point, and excluded
//! from the applied graph** -- never silently deleted. The flag clears
//! automatically when the endpoint resolves again. The bridge's sweep sets the
//! flag; [`apply_overlay`] simply skips any edge/merge that is flagged or whose
//! endpoint is currently absent, so traversal is always structurally safe.

use serde::{Deserialize, Serialize};

use crate::model::{DefSource, Definition, HierGraph, HierNode, NodeId, NodeKind};
use std::collections::HashMap;

/// The persisted **never-reused** `node:<n>` ordinal allocator -- the exact
/// shape + guarantee of `concept_identity.json`'s `next_sem_ordinal` (plan
/// SS2.2). A dropped overlay node's number is never recycled onto a different
/// node, so authored data can never silently re-bind to an unrelated node.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct HierarchyIdentity {
    /// The next `node:` ordinal to mint -- monotonically non-decreasing, never
    /// reused.
    #[serde(default)]
    pub next_node_ordinal: u32,
}

impl HierarchyIdentity {
    /// Mint the next overlay-only [`NodeId`] (`node:<n>`, zero-padded to match
    /// `sem:0007`) and bump the high-water mark. The minted number is never
    /// handed out again.
    pub fn mint(&mut self) -> NodeId {
        let id = NodeId(format!("node:{:04}", self.next_node_ordinal));
        self.next_node_ordinal += 1;
        id
    }
}

/// One overlay record for a node. Either **overrides** an existing projected
/// node (its `id` matches a `concept:` / `vocab:` node) or **introduces** a
/// brand-new authored node (a minted `node:<n>` id). `definition: None` ⇒ the
/// node inherits its definition from the backing record (an override that
/// carries only a `label_override`, or a placeholder authored node awaiting a
/// definition).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OverlayNode {
    pub id: NodeId,
    /// Authored definition text. `None` ⇒ inherit (fall through the ladder).
    #[serde(default)]
    pub definition: Option<String>,
    /// Which authored rung `definition` sits on -- [`DefSource::Authored`]
    /// (top priority) or [`DefSource::LlmDraft`] (below the inherited
    /// description). Ignored when `definition` is `None`.
    #[serde(default = "authored_default")]
    pub definition_source: DefSource,
    /// An authored label override. `None` ⇒ keep the projected label.
    #[serde(default)]
    pub label_override: Option<String>,
    #[serde(default)]
    pub created_at: f64,
    #[serde(default)]
    pub updated_at: f64,
}

/// The serde default for [`OverlayNode::definition_source`] -- `Authored`, the
/// top rung. (LLM drafting is deferred, OQ-3 default = manual-only.)
fn authored_default() -> DefSource {
    DefSource::Authored
}

impl OverlayNode {
    /// True when this record carries no authored data at all -- no definition
    /// and no label override. Such a record is pruned rather than persisted, so
    /// the overlay stays minimal (and the removal test sees an empty file once
    /// the last authored datum is cleared).
    pub fn is_empty(&self) -> bool {
        self.definition
            .as_deref()
            .map(str::trim)
            .unwrap_or("")
            .is_empty()
            && self
                .label_override
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
    }
}

/// An authored containment edge: `parent` contains `child`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OverlayEdge {
    pub parent: NodeId,
    pub child: NodeId,
    #[serde(default)]
    pub created_at: f64,
    /// Endpoint vanished on a recompute -- retained, surfaced, excluded from the
    /// applied graph until it resolves again (the `Relation.dangling` rule).
    #[serde(default)]
    pub dangling: bool,
}

impl OverlayEdge {
    /// Same `(parent, child)` pair, ignoring timestamps + the dangling flag.
    pub fn same_edge(&self, other: &OverlayEdge) -> bool {
        self.parent == other.parent && self.child == other.child
    }
}

/// A merge: declares `primary` and `alias` are the SAME node (OQ-2). On apply,
/// `alias`'s edges + cross-references re-point to `primary` and `alias` is
/// dropped from the graph.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NodeMerge {
    pub primary: NodeId,
    pub alias: NodeId,
    #[serde(default)]
    pub created_at: f64,
    /// Either endpoint vanished -- retained + flagged + excluded, like an edge.
    #[serde(default)]
    pub dangling: bool,
}

impl NodeMerge {
    /// Same `(primary, alias)` pair, ignoring timestamps + the dangling flag.
    pub fn same_merge(&self, other: &NodeMerge) -> bool {
        self.primary == other.primary && self.alias == other.alias
    }
}

/// The whole additive overlay -- a flat, id-linked table (NOT nested), exactly
/// like the projected graph it layers onto. `#[serde(default)]` on every field
/// keeps it migration-free: an older / empty file loads as the empty overlay.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct HierarchyOverlay {
    #[serde(default)]
    pub nodes: Vec<OverlayNode>,
    #[serde(default)]
    pub edges: Vec<OverlayEdge>,
    #[serde(default)]
    pub merges: Vec<NodeMerge>,
}

impl HierarchyOverlay {
    /// An empty overlay -- the fresh-install / fail-soft default. Applying it is
    /// the identity (the projected graph is returned unchanged), so "no authored
    /// data ⇒ behaviour is exactly the projection" holds.
    pub fn empty() -> Self {
        Self::default()
    }

    /// True when nothing has been authored -- the removal-test ground state.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.edges.is_empty() && self.merges.is_empty()
    }

    /// The overlay node for `id`, if one is recorded.
    pub fn node(&self, id: &NodeId) -> Option<&OverlayNode> {
        self.nodes.iter().find(|n| &n.id == id)
    }
}

/// Resolve a definition by the full priority ladder of plan SS1, including the
/// LLM-draft rung that sits *below* the inherited description:
///
/// 1. authored override (`source == Authored`, non-blank);
/// 2. the backing record's inherited description (non-blank);
/// 3. an LLM-drafted fallback (`source == LlmDraft`, non-blank);
/// 4. [`DefSource::Missing`].
///
/// `inherited_kind` is the rung the inherited text sits on
/// ([`DefSource::InheritedConcept`] / [`DefSource::InheritedAnchor`]); it is
/// only consulted when the inherited text is used.
fn resolve_overlay_definition(
    authored: Option<&str>,
    authored_source: DefSource,
    inherited: Option<&str>,
    inherited_kind: DefSource,
) -> Definition {
    let authored_ok = authored.map(|a| !a.trim().is_empty()).unwrap_or(false);
    // Rung 1: a true authored override wins outright.
    if authored_ok && authored_source == DefSource::Authored {
        return Definition {
            text: authored.unwrap().to_owned(),
            source: DefSource::Authored,
        };
    }
    // Rung 2: the backing record's description.
    if let Some(i) = inherited {
        if !i.trim().is_empty() {
            return Definition {
                text: i.to_owned(),
                source: inherited_kind,
            };
        }
    }
    // Rung 3: an LLM draft only fills when nothing better exists.
    if authored_ok && authored_source == DefSource::LlmDraft {
        return Definition {
            text: authored.unwrap().to_owned(),
            source: DefSource::LlmDraft,
        };
    }
    Definition::missing()
}

/// The inherited text + rung a projected node currently carries, recovered from
/// its resolved [`Definition`]. A node projected from a backing record has an
/// `Inherited*` source whose `text` IS the inherited description; a `Missing`
/// node has no inherited text. (Authored/LlmDraft never appear pre-overlay.)
fn inherited_of(node: &HierNode) -> (Option<String>, DefSource) {
    match node.definition.source {
        DefSource::InheritedConcept | DefSource::InheritedAnchor => {
            (Some(node.definition.text.clone()), node.definition.source)
        }
        // No inherited backing text; keep a sensible rung for the (unused) param.
        _ => (None, node.definition.source),
    }
}

/// Layer a [`HierarchyOverlay`] onto a projected [`HierGraph`], producing the
/// final applied graph (plan SS2.2). Pure -- no I/O, no mutation of the inputs.
///
/// Order of operations:
/// 1. **Authored nodes** -- override an existing node's definition/label by the
///    full ladder, or introduce a brand-new [`NodeKind::Authored`] node.
/// 2. **Authored containment edges** -- add `parent -> child` (both endpoints
///    recorded), skipping dangling / unresolved / self / duplicate edges.
/// 3. **Merges** -- fold each `alias` into its `primary` (re-point edges +
///    cross-references, union containment, inherit a definition only if the
///    primary lacks one), skipping dangling / unresolved merges.
/// 4. **Finalise `is_leaf`** once all containment is assembled.
///
/// Applying [`HierarchyOverlay::empty`] returns the projection unchanged.
pub fn apply_overlay(mut graph: HierGraph, overlay: &HierarchyOverlay) -> HierGraph {
    // -- 1. Authored nodes: overrides + brand-new authored nodes. --
    for on in &overlay.nodes {
        match graph.nodes.iter().position(|n| n.id == on.id) {
            Some(idx) => {
                let (inherited, inherited_kind) = inherited_of(&graph.nodes[idx]);
                let node = &mut graph.nodes[idx];
                node.definition = resolve_overlay_definition(
                    on.definition.as_deref(),
                    on.definition_source,
                    inherited.as_deref(),
                    inherited_kind,
                );
                if let Some(lbl) = on.label_override.as_deref() {
                    if !lbl.trim().is_empty() {
                        node.label = lbl.to_owned();
                    }
                }
            }
            None => {
                // A brand-new authored node the source stores never produced.
                let definition = resolve_overlay_definition(
                    on.definition.as_deref(),
                    on.definition_source,
                    None,
                    DefSource::Missing,
                );
                let label = on
                    .label_override
                    .as_deref()
                    .filter(|l| !l.trim().is_empty())
                    .map(str::to_owned)
                    .unwrap_or_else(|| default_authored_label(&on.id));
                graph.nodes.push(HierNode {
                    id: on.id.clone(),
                    label,
                    definition,
                    kind: NodeKind::Authored,
                    parents: Vec::new(),
                    children: Vec::new(),
                    embedding: None,
                    is_leaf: true,
                });
            }
        }
    }

    // Re-index after node insertion so edges + merges can resolve authored ids.
    let index = build_index(&graph);

    // -- 2. Authored containment edges. --
    for e in overlay.edges.iter().filter(|e| !e.dangling) {
        add_containment(&mut graph.nodes, &index, &e.parent, &e.child);
    }

    // -- 3. Merges: fold alias into primary. --
    for m in overlay.merges.iter().filter(|m| !m.dangling) {
        apply_merge(&mut graph, &m.primary, &m.alias);
    }

    // -- 4. Finalise leaf flags. --
    for n in graph.nodes.iter_mut() {
        n.is_leaf = n.children.is_empty();
    }

    graph
}

/// A default label for an authored node with no label override -- its ordinal,
/// so the UI shows something stable rather than the raw `node:` id.
fn default_authored_label(id: &NodeId) -> String {
    match id.split() {
        Some((_, ord)) => format!("Node {ord}"),
        None => id.as_str().to_owned(),
    }
}

/// Build a fresh `id -> index` map over the current node table.
fn build_index(graph: &HierGraph) -> HashMap<NodeId, usize> {
    graph
        .nodes
        .iter()
        .enumerate()
        .map(|(i, n)| (n.id.clone(), i))
        .collect()
}

/// Add a containment edge `parent -> child` on both endpoints, skipping a
/// self-edge, a dangling endpoint, or a duplicate. (Mirrors the projection's
/// own `add_containment`.)
fn add_containment(
    nodes: &mut [HierNode],
    index: &HashMap<NodeId, usize>,
    parent: &NodeId,
    child: &NodeId,
) {
    if parent == child {
        return;
    }
    let (Some(&pi), Some(&ci)) = (index.get(parent), index.get(child)) else {
        return;
    };
    if !nodes[ci].parents.contains(parent) {
        nodes[ci].parents.push(parent.clone());
    }
    if !nodes[pi].children.contains(child) {
        nodes[pi].children.push(child.clone());
    }
}

/// Fold `alias` into `primary`: re-point every containment edge + cross-
/// reference that touches `alias` onto `primary`, union the containment
/// neighbours, inherit `alias`'s definition only when `primary` has none, then
/// drop `alias` from the node table. No-op when either id is absent or they are
/// equal.
fn apply_merge(graph: &mut HierGraph, primary: &NodeId, alias: &NodeId) {
    if primary == alias {
        return;
    }
    if graph.node(primary).is_none() || graph.node(alias).is_none() {
        return;
    }

    // Inherit the alias's definition into the primary if the primary lacks one.
    let alias_def = graph.node(alias).map(|n| n.definition.clone());
    if let Some(p) = graph.nodes.iter_mut().find(|n| &n.id == primary) {
        if p.definition.is_missing() {
            if let Some(d) = alias_def {
                if !d.is_missing() {
                    p.definition = d;
                }
            }
        }
    }

    // Re-point cross-references: any endpoint == alias becomes primary, dropping
    // a self-reference + de-duping.
    for x in graph.xrefs.iter_mut() {
        if &x.from == alias {
            x.from = primary.clone();
        }
        if &x.to == alias {
            x.to = primary.clone();
        }
    }
    graph.xrefs.retain(|x| x.from != x.to);
    dedup_xrefs(&mut graph.xrefs);

    // Re-point containment in every node's parent/child lists.
    for n in graph.nodes.iter_mut() {
        repoint(&mut n.parents, alias, primary);
        repoint(&mut n.children, alias, primary);
    }

    // Union the alias's own neighbours onto the primary, then remove the alias.
    let (alias_parents, alias_children) = graph
        .node(alias)
        .map(|n| (n.parents.clone(), n.children.clone()))
        .unwrap_or_default();
    if let Some(p) = graph.nodes.iter_mut().find(|n| &n.id == primary) {
        for pa in alias_parents {
            if pa != *primary && !p.parents.contains(&pa) {
                p.parents.push(pa);
            }
        }
        for ch in alias_children {
            if ch != *primary && !p.children.contains(&ch) {
                p.children.push(ch);
            }
        }
        // A self-edge can appear if primary and alias shared a neighbour.
        p.parents.retain(|x| x != primary);
        p.children.retain(|x| x != primary);
    }
    graph.nodes.retain(|n| &n.id != alias);
}

/// Replace every `from` with `to` in a node-id list, de-duping.
fn repoint(list: &mut Vec<NodeId>, from: &NodeId, to: &NodeId) {
    let mut seen = false;
    for id in list.iter_mut() {
        if id == from {
            *id = to.clone();
        }
    }
    list.retain(|id| {
        if id == to {
            if seen {
                return false;
            }
            seen = true;
        }
        true
    });
}

/// De-dupe cross-reference edges in place, preserving order.
fn dedup_xrefs(xrefs: &mut Vec<crate::model::XRef>) {
    let mut seen = std::collections::HashSet::new();
    xrefs.retain(|x| seen.insert(x.clone()));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{XRef, XRefKind};

    /// A projected concept node with an inherited definition.
    fn concept_node(id: &str, def: &str) -> HierNode {
        HierNode {
            id: NodeId::concept(id),
            label: id.to_uppercase(),
            definition: Definition {
                text: def.to_owned(),
                source: DefSource::InheritedConcept,
            },
            kind: NodeKind::Concept,
            parents: Vec::new(),
            children: Vec::new(),
            embedding: None,
            is_leaf: true,
        }
    }

    /// A projected node with no definition (flagged Missing).
    fn missing_node(id: &str) -> HierNode {
        HierNode {
            id: NodeId::concept(id),
            label: id.to_uppercase(),
            definition: Definition::missing(),
            kind: NodeKind::Concept,
            parents: Vec::new(),
            children: Vec::new(),
            embedding: None,
            is_leaf: true,
        }
    }

    fn graph(nodes: Vec<HierNode>) -> HierGraph {
        HierGraph {
            nodes,
            xrefs: Vec::new(),
        }
    }

    #[test]
    fn identity_mints_monotonic_never_reused() {
        let mut id = HierarchyIdentity::default();
        assert_eq!(id.mint().as_str(), "node:0000");
        assert_eq!(id.mint().as_str(), "node:0001");
        assert_eq!(id.next_node_ordinal, 2);
    }

    #[test]
    fn empty_overlay_is_identity() {
        let g = graph(vec![concept_node("a", "alpha")]);
        let out = apply_overlay(g.clone(), &HierarchyOverlay::empty());
        assert_eq!(out, g, "applying an empty overlay changes nothing");
    }

    #[test]
    fn authored_definition_overrides_inherited() {
        let g = graph(vec![concept_node("a", "from record")]);
        let overlay = HierarchyOverlay {
            nodes: vec![OverlayNode {
                id: NodeId::concept("a"),
                definition: Some("hand-written".into()),
                definition_source: DefSource::Authored,
                label_override: Some("Alpha".into()),
                created_at: 1.0,
                updated_at: 1.0,
            }],
            ..Default::default()
        };
        let out = apply_overlay(g, &overlay);
        let n = &out.nodes[0];
        assert_eq!(n.definition.source, DefSource::Authored);
        assert_eq!(n.definition.text, "hand-written");
        assert_eq!(n.label, "Alpha", "label override applied");
    }

    #[test]
    fn llm_draft_sits_below_inherited_but_fills_missing() {
        // With an inherited description present, an LLM draft does NOT win.
        let g = graph(vec![concept_node("a", "real description")]);
        let draft = HierarchyOverlay {
            nodes: vec![OverlayNode {
                id: NodeId::concept("a"),
                definition: Some("ai guess".into()),
                definition_source: DefSource::LlmDraft,
                label_override: None,
                created_at: 0.0,
                updated_at: 0.0,
            }],
            ..Default::default()
        };
        let out = apply_overlay(g, &draft);
        assert_eq!(out.nodes[0].definition.source, DefSource::InheritedConcept);

        // But on a Missing node the draft fills the gap (rung 3).
        let g2 = graph(vec![missing_node("b")]);
        let draft2 = HierarchyOverlay {
            nodes: vec![OverlayNode {
                id: NodeId::concept("b"),
                definition: Some("ai guess".into()),
                definition_source: DefSource::LlmDraft,
                label_override: None,
                created_at: 0.0,
                updated_at: 0.0,
            }],
            ..Default::default()
        };
        let out2 = apply_overlay(g2, &draft2);
        assert_eq!(out2.nodes[0].definition.source, DefSource::LlmDraft);
        assert_eq!(out2.nodes[0].definition.text, "ai guess");
    }

    #[test]
    fn authored_node_is_introduced_with_authored_kind() {
        let g = graph(vec![concept_node("a", "alpha")]);
        let overlay = HierarchyOverlay {
            nodes: vec![OverlayNode {
                id: NodeId("node:0000".into()),
                definition: Some("a net-new theme".into()),
                definition_source: DefSource::Authored,
                label_override: Some("Theme".into()),
                created_at: 0.0,
                updated_at: 0.0,
            }],
            ..Default::default()
        };
        let out = apply_overlay(g, &overlay);
        let n = out
            .nodes
            .iter()
            .find(|n| n.id == NodeId("node:0000".into()))
            .unwrap();
        assert_eq!(n.kind, NodeKind::Authored);
        assert_eq!(n.label, "Theme");
        assert_eq!(n.definition.text, "a net-new theme");
    }

    #[test]
    fn authored_node_without_label_gets_ordinal_label() {
        let overlay = HierarchyOverlay {
            nodes: vec![OverlayNode {
                id: NodeId("node:0007".into()),
                definition: Some("d".into()),
                definition_source: DefSource::Authored,
                label_override: None,
                created_at: 0.0,
                updated_at: 0.0,
            }],
            ..Default::default()
        };
        let out = apply_overlay(graph(vec![]), &overlay);
        assert_eq!(out.nodes[0].label, "Node 0007");
    }

    #[test]
    fn authored_edge_adds_containment_both_ways() {
        let g = graph(vec![
            concept_node("parent", "p"),
            concept_node("child", "c"),
        ]);
        let overlay = HierarchyOverlay {
            edges: vec![OverlayEdge {
                parent: NodeId::concept("parent"),
                child: NodeId::concept("child"),
                created_at: 0.0,
                dangling: false,
            }],
            ..Default::default()
        };
        let out = apply_overlay(g, &overlay);
        let child = out
            .nodes
            .iter()
            .find(|n| n.id == NodeId::concept("child"))
            .unwrap();
        let parent = out
            .nodes
            .iter()
            .find(|n| n.id == NodeId::concept("parent"))
            .unwrap();
        assert_eq!(child.parents, vec![NodeId::concept("parent")]);
        assert_eq!(parent.children, vec![NodeId::concept("child")]);
        assert!(!parent.is_leaf, "parent now has a child");
        assert!(child.is_leaf);
    }

    #[test]
    fn dangling_or_unresolved_edge_is_skipped() {
        let g = graph(vec![concept_node("a", "alpha")]);
        let overlay = HierarchyOverlay {
            edges: vec![
                // Flagged dangling -> skipped.
                OverlayEdge {
                    parent: NodeId::concept("a"),
                    child: NodeId::concept("gone"),
                    created_at: 0.0,
                    dangling: true,
                },
                // Endpoint not in the graph -> skipped (defence in depth).
                OverlayEdge {
                    parent: NodeId::concept("a"),
                    child: NodeId::concept("ghost"),
                    created_at: 0.0,
                    dangling: false,
                },
            ],
            ..Default::default()
        };
        let out = apply_overlay(g, &overlay);
        assert!(out.nodes[0].children.is_empty(), "no phantom child edge");
        assert_eq!(out.nodes.len(), 1, "no phantom node invented");
    }

    #[test]
    fn merge_folds_alias_into_primary() {
        // concept:c  ~names~> vocab:v ; vocab:v has a child leaf.
        let mut g = HierGraph {
            nodes: vec![
                concept_node("c", "the concept"),
                HierNode {
                    id: NodeId::vocab("v"),
                    label: "v".into(),
                    definition: Definition {
                        text: "the vocab term".into(),
                        source: DefSource::InheritedAnchor,
                    },
                    kind: NodeKind::Vocab,
                    parents: Vec::new(),
                    children: vec![NodeId::vocab("leaf")],
                    embedding: None,
                    is_leaf: false,
                },
                HierNode {
                    id: NodeId::vocab("leaf"),
                    label: "leaf".into(),
                    definition: Definition {
                        text: "x".into(),
                        source: DefSource::InheritedAnchor,
                    },
                    kind: NodeKind::Vocab,
                    parents: vec![NodeId::vocab("v")],
                    children: Vec::new(),
                    embedding: None,
                    is_leaf: true,
                },
            ],
            xrefs: vec![XRef {
                from: NodeId::concept("c"),
                to: NodeId::vocab("v"),
                kind: XRefKind::Names,
            }],
        };
        // Sanity: pre-merge the names xref points at v.
        assert_eq!(g.xrefs[0].to, NodeId::vocab("v"));

        let overlay = HierarchyOverlay {
            merges: vec![NodeMerge {
                primary: NodeId::concept("c"),
                alias: NodeId::vocab("v"),
                created_at: 0.0,
                dangling: false,
            }],
            ..Default::default()
        };
        g = apply_overlay(g, &overlay);

        // The alias is gone; the primary absorbed v's child.
        assert!(g.node(&NodeId::vocab("v")).is_none(), "alias removed");
        let c = g.node(&NodeId::concept("c")).unwrap();
        assert!(
            c.children.contains(&NodeId::vocab("leaf")),
            "primary absorbed the child"
        );
        // The leaf's parent re-pointed to the primary.
        let leaf = g.node(&NodeId::vocab("leaf")).unwrap();
        assert_eq!(leaf.parents, vec![NodeId::concept("c")]);
        // The self-referential names xref (c ~names~> c) is dropped.
        assert!(
            g.xrefs.is_empty(),
            "the names xref collapsed to a self-edge and was dropped"
        );
    }

    #[test]
    fn merge_inherits_definition_only_when_primary_missing() {
        let g = HierGraph {
            nodes: vec![missing_node("p"), concept_node("a", "alias def")],
            xrefs: Vec::new(),
        };
        let overlay = HierarchyOverlay {
            merges: vec![NodeMerge {
                primary: NodeId::concept("p"),
                alias: NodeId::concept("a"),
                created_at: 0.0,
                dangling: false,
            }],
            ..Default::default()
        };
        let out = apply_overlay(g, &overlay);
        let p = out.node(&NodeId::concept("p")).unwrap();
        assert_eq!(
            p.definition.text, "alias def",
            "missing primary took the alias definition"
        );
    }

    #[test]
    fn dangling_merge_is_skipped() {
        let g = graph(vec![concept_node("p", "p")]);
        let overlay = HierarchyOverlay {
            merges: vec![NodeMerge {
                primary: NodeId::concept("p"),
                alias: NodeId::concept("gone"),
                created_at: 0.0,
                dangling: true,
            }],
            ..Default::default()
        };
        let out = apply_overlay(g, &overlay);
        assert_eq!(
            out.nodes.len(),
            1,
            "dangling merge left the graph untouched"
        );
    }

    #[test]
    fn overlay_serde_round_trips_and_defaults() {
        // A minimal object (older shape) loads as empty via serde defaults.
        let o: HierarchyOverlay = serde_json::from_value(serde_json::json!({})).unwrap();
        assert!(o.is_empty());

        let full = HierarchyOverlay {
            nodes: vec![OverlayNode {
                id: NodeId("node:0000".into()),
                definition: Some("d".into()),
                definition_source: DefSource::Authored,
                label_override: None,
                created_at: 1.0,
                updated_at: 2.0,
            }],
            edges: vec![OverlayEdge {
                parent: NodeId::concept("a"),
                child: NodeId::concept("b"),
                created_at: 1.0,
                dangling: false,
            }],
            merges: vec![NodeMerge {
                primary: NodeId::concept("a"),
                alias: NodeId::vocab("v"),
                created_at: 1.0,
                dangling: false,
            }],
        };
        let v = serde_json::to_value(&full).unwrap();
        assert_eq!(v["nodes"][0]["definition_source"], "authored");
        let back: HierarchyOverlay = serde_json::from_value(v).unwrap();
        assert_eq!(back, full);
    }

    #[test]
    fn is_empty_node_detects_no_authored_data() {
        let bare = OverlayNode {
            id: NodeId::concept("a"),
            definition: Some("   ".into()),
            definition_source: DefSource::Authored,
            label_override: None,
            created_at: 0.0,
            updated_at: 0.0,
        };
        assert!(bare.is_empty(), "whitespace-only def + no label => empty");
    }
}
