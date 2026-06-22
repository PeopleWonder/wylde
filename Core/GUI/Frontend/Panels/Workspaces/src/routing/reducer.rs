//! Pure view-model for the Relations editor (concept-routing **R1.5c**) — the
//! testable logic the gpui [`super::RelationsView`] renders, kept free of gpui
//! so it unit-tests without a window: the four authoring **groups** (addendum
//! §2.2 mock), edge-kind metadata (glyph + the kind it authors), picker
//! candidate filtering, the whole-graph **overview** bucketing (the seam R3's
//! typed-edge tree reuses), and verb-error → inline-message mapping.

use std::collections::HashMap;

use super::ipc::{NodeItem, NodeRefView, RelationKindView, RelationView};

/// The four authoring groups in the node-focused editor (addendum §2.2):
///
/// ```text
/// DEPENDS ON          → focus depends-on X            (dependency, focus = from)
/// DEPENDED ON BY      ← X depends-on focus            (read-only backward view)
/// RELATES TO          ↔ focus relates-to X            (positive, symmetric)
/// IS NOT              ⊘ focus excludes X              (negative, symmetric)
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RelGroup {
    DependsOn,
    DependedOnBy,
    RelatesTo,
    IsNot,
}

impl RelGroup {
    /// The groups in display order (the mock's top-to-bottom).
    pub const ORDER: [RelGroup; 4] = [
        RelGroup::DependsOn,
        RelGroup::DependedOnBy,
        RelGroup::RelatesTo,
        RelGroup::IsNot,
    ];

    /// The section header.
    pub fn header(self) -> &'static str {
        match self {
            RelGroup::DependsOn => "DEPENDS ON",
            RelGroup::DependedOnBy => "DEPENDED ON BY",
            RelGroup::RelatesTo => "RELATES TO",
            RelGroup::IsNot => "IS NOT",
        }
    }

    /// The arrow/severed glyph that prefixes each edge in this group — the
    /// primary visual distinction between the kinds (esp. `⊘` exclusion vs `→`
    /// dependency, the addendum's emphasis).
    pub fn glyph(self) -> &'static str {
        match self {
            RelGroup::DependsOn => "→",
            RelGroup::DependedOnBy => "←",
            RelGroup::RelatesTo => "↔",
            RelGroup::IsNot => "⊘",
        }
    }

    /// A one-line explainer for the section.
    pub fn hint(self) -> &'static str {
        match self {
            RelGroup::DependsOn => "pulls these in when the focus activates (transitive)",
            RelGroup::DependedOnBy => "the focus's blast radius — author it from the other node",
            RelGroup::RelatesTo => "gentle two-way co-activation",
            RelGroup::IsNot => "soft exclusion — tells routing what NOT to conflate",
        }
    }

    /// Whether the user can author into this group from the focus node. The
    /// backward dependency view is read-only here (you write it by focusing the
    /// *other* node and adding a DEPENDS ON edge — addendum §2.2).
    pub fn is_authorable(self) -> bool {
        !matches!(self, RelGroup::DependedOnBy)
    }

    /// The kind a new edge in this group carries.
    pub fn kind(self) -> RelationKindView {
        match self {
            RelGroup::DependsOn | RelGroup::DependedOnBy => RelationKindView::Dependency,
            RelGroup::RelatesTo => RelationKindView::Positive,
            RelGroup::IsNot => RelationKindView::Negative,
        }
    }
}

/// One edge as shown in a group: the *other* endpoint (not the focus), the
/// optional note, and the `(from,to,kind)` needed to remove it. `other` is what
/// the row labels + deep-links to.
#[derive(Clone, Debug, PartialEq)]
pub struct GroupEdge {
    pub other: NodeRefView,
    pub note: Option<String>,
    pub from: NodeRefView,
    pub to: NodeRefView,
    pub kind: RelationKindView,
}

/// Bucket the edges touching `focus` (the `relations.list` set) into the four
/// authoring groups. Positive/Negative are symmetric (the focus may be stored
/// as either endpoint — we surface the *other* end); Dependency splits by
/// direction: `focus → X` is DEPENDS ON, `X → focus` is DEPENDED ON BY.
pub fn group_edges(focus: &NodeRefView, touching: &[RelationView]) -> Vec<(RelGroup, Vec<GroupEdge>)> {
    let mut depends_on = Vec::new();
    let mut depended_on_by = Vec::new();
    let mut relates_to = Vec::new();
    let mut is_not = Vec::new();

    for r in touching {
        let other = if &r.from == focus { &r.to } else { &r.from };
        let edge = GroupEdge {
            other: other.clone(),
            note: r.note.clone(),
            from: r.from.clone(),
            to: r.to.clone(),
            kind: r.kind,
        };
        match r.kind {
            RelationKindView::Dependency if &r.from == focus => depends_on.push(edge),
            RelationKindView::Dependency => depended_on_by.push(edge),
            RelationKindView::Positive => relates_to.push(edge),
            RelationKindView::Negative => is_not.push(edge),
        }
    }

    vec![
        (RelGroup::DependsOn, depends_on),
        (RelGroup::DependedOnBy, depended_on_by),
        (RelGroup::RelatesTo, relates_to),
        (RelGroup::IsNot, is_not),
    ]
}

/// Filter the node universe to the candidates a `[+ add]` picker should offer
/// for `focus` in `group`: drop the focus itself, drop nodes already related to
/// the focus *in that group's kind* (so you can't double-author the same edge),
/// and case-insensitively match `query` against the label. Result keeps the
/// universe's order (concepts then vocab, each label-sorted).
pub fn picker_candidates<'a>(
    universe: &'a [NodeItem],
    focus: &NodeRefView,
    already: &[NodeRefView],
    query: &str,
) -> Vec<&'a NodeItem> {
    let q = query.trim().to_lowercase();
    universe
        .iter()
        .filter(|item| &item.node != focus)
        .filter(|item| !already.contains(&item.node))
        .filter(|item| q.is_empty() || item.label.to_lowercase().contains(&q))
        .collect()
}

/// Resolve a node to its human label using the universe (a concept shows its
/// label, not its store id). Falls back to a tagged raw key when the node isn't
/// in the universe (e.g. a dangling concept id) so a row never renders blank.
pub fn label_for(node: &NodeRefView, universe: &[NodeItem]) -> String {
    if let Some(item) = universe.iter().find(|i| &i.node == node) {
        return item.label.clone();
    }
    match node {
        NodeRefView::Concept { id } => format!("concept: {id}"),
        NodeRefView::Vocab { identifier } => format!("{{{{{identifier}}}}}"),
    }
}

/// One row of the whole-graph **overview** (no focus selected): a from-node and
/// the edges that originate from / pair with it, grouped by kind. This is the
/// structure R3's typed-edge tree renders from, so the overview and the tree
/// read the same shape.
#[derive(Clone, Debug, PartialEq)]
pub struct OverviewRow {
    pub node: NodeRefView,
    pub edges: Vec<RelationView>,
}

/// Bucket the whole graph by `from` node, in first-seen order, so the overview
/// lists each node once with its outgoing/symmetric edges beneath it.
pub fn overview(relations: &[RelationView]) -> Vec<OverviewRow> {
    let mut order: Vec<NodeRefView> = Vec::new();
    let mut at: HashMap<NodeRefView, usize> = HashMap::new();
    let mut rows: Vec<OverviewRow> = Vec::new();
    for r in relations {
        let idx = *at.entry(r.from.clone()).or_insert_with(|| {
            order.push(r.from.clone());
            rows.push(OverviewRow {
                node: r.from.clone(),
                edges: Vec::new(),
            });
            rows.len() - 1
        });
        rows[idx].edges.push(r.clone());
    }
    rows
}

/// Map a verb-error string to a clean, inline message (addendum §2.1
/// validation). The pipe surfaces `code: message`; we lead with the
/// human-meaningful cause so the editor's status strip reads plainly.
pub fn explain_error(err: &str) -> String {
    if err.contains("already_exists") {
        "That relation already exists.".to_owned()
    } else if err.contains("cannot connect a node to itself") {
        "A relation can't connect a node to itself.".to_owned()
    } else if err.contains("unknown `from` node") || err.contains("unknown `to` node") {
        "One of the nodes no longer exists in this workspace.".to_owned()
    } else if err.contains("bad_request") {
        // Strip the `bad_request:` code prefix; show the handler's message.
        let msg = err.split_once(':').map(|(_, m)| m).unwrap_or(err).trim();
        format!("Invalid relation: {msg}")
    } else {
        format!("Couldn't save the relation: {err}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nc() -> NodeRefView {
        NodeRefView::concept("nextcloud")
    }
    fn ddns() -> NodeRefView {
        NodeRefView::vocab("ddns")
    }
    fn wylde() -> NodeRefView {
        NodeRefView::concept("wylde")
    }

    fn rel(from: NodeRefView, to: NodeRefView, kind: RelationKindView) -> RelationView {
        RelationView {
            from,
            to,
            kind,
            note: None,
            created_at: 0.0,
        }
    }

    #[test]
    fn groups_split_dependency_by_direction() {
        let focus = nc();
        let touching = vec![
            rel(nc(), ddns(), RelationKindView::Dependency), // focus → ddns  : DEPENDS ON
            rel(wylde(), nc(), RelationKindView::Dependency), // wylde → focus : DEPENDED ON BY
            rel(nc(), wylde(), RelationKindView::Negative),  // symmetric     : IS NOT
        ];
        let groups = group_edges(&focus, &touching);
        let get = |g: RelGroup| {
            groups
                .iter()
                .find(|(k, _)| *k == g)
                .map(|(_, v)| v.clone())
                .unwrap()
        };
        let deps = get(RelGroup::DependsOn);
        assert_eq!(deps.len(), 1);
        assert_eq!(deps[0].other, ddns());

        let blast = get(RelGroup::DependedOnBy);
        assert_eq!(blast.len(), 1);
        assert_eq!(blast[0].other, wylde(), "the other end is shown");

        assert_eq!(get(RelGroup::IsNot).len(), 1);
        assert!(get(RelGroup::RelatesTo).is_empty());
    }

    #[test]
    fn symmetric_edge_surfaces_other_end_regardless_of_orientation() {
        let focus = nc();
        // Stored with focus as `to` (canonicalised orientation) — still shows
        // the OTHER endpoint, not the focus.
        let touching = vec![rel(wylde(), nc(), RelationKindView::Negative)];
        let groups = group_edges(&focus, &touching);
        let is_not = &groups.iter().find(|(k, _)| *k == RelGroup::IsNot).unwrap().1;
        assert_eq!(is_not[0].other, wylde());
    }

    #[test]
    fn depended_on_by_is_read_only() {
        assert!(!RelGroup::DependedOnBy.is_authorable());
        assert!(RelGroup::DependsOn.is_authorable());
        assert!(RelGroup::RelatesTo.is_authorable());
        assert!(RelGroup::IsNot.is_authorable());
    }

    #[test]
    fn group_kinds_map_to_the_verb() {
        assert_eq!(RelGroup::DependsOn.kind(), RelationKindView::Dependency);
        assert_eq!(RelGroup::RelatesTo.kind(), RelationKindView::Positive);
        assert_eq!(RelGroup::IsNot.kind(), RelationKindView::Negative);
    }

    fn universe() -> Vec<NodeItem> {
        vec![
            NodeItem {
                node: nc(),
                label: "Nextcloud".into(),
            },
            NodeItem {
                node: wylde(),
                label: "Wylde".into(),
            },
            NodeItem {
                node: ddns(),
                label: "{{ddns}}".into(),
            },
        ]
    }

    #[test]
    fn picker_drops_self_and_already_related_and_matches_query() {
        let u = universe();
        // Exclude focus (nextcloud) + already-related (wylde); query "d" keeps ddns.
        let cands = picker_candidates(&u, &nc(), &[wylde()], "d");
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].node, ddns());

        // Empty query → everything except self + already.
        let all = picker_candidates(&u, &nc(), &[], "");
        assert_eq!(all.len(), 2);
        assert!(all.iter().all(|i| i.node != nc()));
    }

    #[test]
    fn label_resolves_concept_to_its_label_with_fallbacks() {
        let u = universe();
        assert_eq!(label_for(&nc(), &u), "Nextcloud");
        assert_eq!(label_for(&ddns(), &u), "{{ddns}}");
        // Not in the universe → tagged fallback (never blank).
        assert_eq!(
            label_for(&NodeRefView::concept("ghost"), &u),
            "concept: ghost"
        );
        assert_eq!(
            label_for(&NodeRefView::vocab("orphan"), &u),
            "{{orphan}}"
        );
    }

    #[test]
    fn overview_buckets_by_from_node_in_first_seen_order() {
        let rels = vec![
            rel(nc(), ddns(), RelationKindView::Dependency),
            rel(nc(), wylde(), RelationKindView::Negative),
            rel(wylde(), ddns(), RelationKindView::Positive),
        ];
        let rows = overview(&rels);
        assert_eq!(rows.len(), 2, "two distinct from-nodes");
        assert_eq!(rows[0].node, nc());
        assert_eq!(rows[0].edges.len(), 2);
        assert_eq!(rows[1].node, wylde());
        assert_eq!(rows[1].edges.len(), 1);
    }

    #[test]
    fn error_messages_are_clean_and_specific() {
        assert_eq!(
            explain_error("already_exists: this relation already exists"),
            "That relation already exists."
        );
        assert_eq!(
            explain_error("bad_request: a relation cannot connect a node to itself"),
            "A relation can't connect a node to itself."
        );
        assert_eq!(
            explain_error("bad_request: unknown `to` node: concept:ghost"),
            "One of the nodes no longer exists in this workspace."
        );
        // Generic bad_request strips the code prefix.
        assert_eq!(
            explain_error("bad_request: kind must be positive | negative | dependency"),
            "Invalid relation: kind must be positive | negative | dependency"
        );
        // Transport/unknown error keeps the detail.
        assert!(explain_error("transport: pipe closed").contains("pipe closed"));
    }
}
