//! Vocabulary list model (Slice N, Build Order §4 `vocabulary/list_view`):
//! merge the workspace + global stores into filterable, sortable rows. Pure
//! and gpui-free; the tab renders [`VocabRow`]s and routes clicks.

use std::collections::HashSet;

use super::ipc::{AnchorScopeTag, AnchorView};

/// Recommended Cleanup threshold (Plan §4.7: unused > N months, default 6,
/// configurable — the Settings knob lands with the §9 settings
/// consolidation; this is the one place the number lives).
pub const CLEANUP_UNUSED_SECS: f64 = 6.0 * 30.44 * 86_400.0;

/// Scope filter for the list.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ScopeFilter {
    #[default]
    All,
    Workspace,
    Global,
}

impl ScopeFilter {
    pub fn label(self) -> &'static str {
        match self {
            ScopeFilter::All => "All",
            ScopeFilter::Workspace => "Workspace",
            ScopeFilter::Global => "Global",
        }
    }

    pub const CYCLE: [ScopeFilter; 3] = [
        ScopeFilter::All,
        ScopeFilter::Workspace,
        ScopeFilter::Global,
    ];
}

/// Which view of the vocabulary the list shows (Slice N stage 4).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ViewFilter {
    /// The living vocabulary (archived hidden).
    #[default]
    Active,
    /// Recommended Cleanup (OI-21): active anchors unused > the threshold.
    Cleanup,
    /// Stale-mark: anchors whose code-symbol target no longer resolves.
    Stale,
    /// Archived anchors (recoverable — never silently decayed).
    Archived,
}

impl ViewFilter {
    pub fn label(self) -> &'static str {
        match self {
            ViewFilter::Active => "Active",
            ViewFilter::Cleanup => "Cleanup",
            ViewFilter::Stale => "Stale",
            ViewFilter::Archived => "Archived",
        }
    }

    pub const CYCLE: [ViewFilter; 4] = [
        ViewFilter::Active,
        ViewFilter::Cleanup,
        ViewFilter::Stale,
        ViewFilter::Archived,
    ];
}

/// One list row: the anchor + which store it came from + the stale badge.
#[derive(Clone, Debug, PartialEq)]
pub struct VocabRow {
    pub anchor: AnchorView,
    pub scope: AnchorScopeTag,
    /// Silent stale badge: the code-symbol target no longer resolves.
    pub stale: bool,
}

impl VocabRow {
    pub fn scope_label(&self) -> &'static str {
        match self.scope {
            AnchorScopeTag::Workspace => "workspace",
            AnchorScopeTag::Global => "global",
        }
    }
}

/// Build the visible rows: merge both stores, apply the scope + view
/// filters and the free-text query (matched against identifier, aliases,
/// description and domain, case-insensitive), then sort by `last_used_at`
/// (desc — the living vocabulary floats up) with identifier as the
/// deterministic tiebreak. `stale` keys are `(scope, identifier)` pairs the
/// tab resolved against the symbol index; `now` is epoch seconds (injected
/// for the Cleanup window test).
#[allow(clippy::too_many_arguments)] // a filter set, not control flow
pub fn rows(
    workspace: &[AnchorView],
    global: &[AnchorView],
    filter: ScopeFilter,
    view: ViewFilter,
    query: &str,
    now: f64,
    stale: &HashSet<(AnchorScopeTag, String)>,
) -> Vec<VocabRow> {
    let q = query.trim().to_lowercase();
    let mut out: Vec<VocabRow> = Vec::new();

    let mut push = |pool: &[AnchorView], scope: AnchorScopeTag| {
        out.extend(pool.iter().cloned().map(|anchor| {
            let stale = stale.contains(&(scope, anchor.identifier.clone()));
            VocabRow {
                anchor,
                scope,
                stale,
            }
        }));
    };
    if filter != ScopeFilter::Global {
        push(workspace, AnchorScopeTag::Workspace);
    }
    if filter != ScopeFilter::Workspace {
        push(global, AnchorScopeTag::Global);
    }

    out.retain(|r| match view {
        ViewFilter::Active => !r.anchor.archived,
        ViewFilter::Cleanup => {
            !r.anchor.archived && now - r.anchor.last_used_at > CLEANUP_UNUSED_SECS
        }
        ViewFilter::Stale => !r.anchor.archived && r.stale,
        ViewFilter::Archived => r.anchor.archived,
    });

    if !q.is_empty() {
        out.retain(|r| matches_query(&r.anchor, &q));
    }

    out.sort_by(|a, b| {
        b.anchor
            .last_used_at
            .partial_cmp(&a.anchor.last_used_at)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.anchor.identifier.cmp(&b.anchor.identifier))
    });
    out
}

/// Re-order rows into a **hierarchy** (thesis S1.2): each root (an anchor with
/// no `parent_anchor`, or whose parent isn't in the visible set) is followed by
/// its descendants depth-first, and every row is paired with its indent depth.
/// Pure + gpui-free; the tab renders the depth as left-indent. Ordering within
/// a sibling group preserves the input order (already recency-sorted by
/// [`rows`]). Cycles / missing parents are handled by treating an
/// already-emitted or unresolved parent as a root, so every row appears exactly
/// once.
pub fn hierarchy_order(rows: &[VocabRow]) -> Vec<(VocabRow, usize)> {
    use std::collections::BTreeMap;

    // identifier → indices of its direct children, in input order.
    let present: HashSet<&str> = rows.iter().map(|r| r.anchor.identifier.as_str()).collect();
    let mut children: BTreeMap<&str, Vec<usize>> = BTreeMap::new();
    let mut roots: Vec<usize> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        match r.anchor.parent_anchor.as_deref() {
            Some(p) if present.contains(p) && p != r.anchor.identifier => {
                children.entry(p).or_default().push(i);
            }
            _ => roots.push(i),
        }
    }

    let mut out: Vec<(VocabRow, usize)> = Vec::with_capacity(rows.len());
    let mut emitted: HashSet<usize> = HashSet::new();
    // Iterative DFS so a pathological deep chain can't blow the stack.
    let mut stack: Vec<(usize, usize)> = roots.iter().rev().map(|&i| (i, 0usize)).collect();
    while let Some((i, depth)) = stack.pop() {
        if !emitted.insert(i) {
            continue; // guard against cycles / double-parenting
        }
        out.push((rows[i].clone(), depth));
        if let Some(kids) = children.get(rows[i].anchor.identifier.as_str()) {
            for &k in kids.iter().rev() {
                if !emitted.contains(&k) {
                    stack.push((k, depth + 1));
                }
            }
        }
    }
    // Safety net: any row not reached (e.g. a parent cycle) is appended flat.
    for (i, r) in rows.iter().enumerate() {
        if emitted.insert(i) {
            out.push((r.clone(), 0));
        }
    }
    out
}

fn matches_query(a: &AnchorView, q: &str) -> bool {
    a.identifier.to_lowercase().contains(q)
        || a.description.to_lowercase().contains(q)
        || a.aliases.iter().any(|al| al.to_lowercase().contains(q))
        || a.domain
            .as_deref()
            .is_some_and(|d| d.to_lowercase().contains(q))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn anchor(id: &str, desc: &str, domain: Option<&str>, last_used: f64) -> AnchorView {
        serde_json::from_value(json!({
            "identifier": id,
            "description": desc,
            "domain": domain,
            "last_used_at": last_used,
            "target": { "type": "concept", "text": desc },
        }))
        .unwrap()
    }

    fn active_rows(
        ws: &[AnchorView],
        gl: &[AnchorView],
        filter: ScopeFilter,
        query: &str,
    ) -> Vec<VocabRow> {
        rows(
            ws,
            gl,
            filter,
            ViewFilter::Active,
            query,
            1_000_000.0,
            &HashSet::new(),
        )
    }

    #[test]
    fn merges_scopes_and_sorts_recent_first() {
        let ws = vec![anchor("alpha", "a", None, 100.0)];
        let gl = vec![
            anchor("beta", "b", None, 300.0),
            anchor("gamma", "c", None, 100.0),
        ];
        let r = active_rows(&ws, &gl, ScopeFilter::All, "");
        assert_eq!(r.len(), 3);
        assert_eq!(r[0].anchor.identifier, "beta", "most recently used first");
        // last_used tie → identifier tiebreak.
        assert_eq!(r[1].anchor.identifier, "alpha");
        assert_eq!(r[2].anchor.identifier, "gamma");
        assert_eq!(r[1].scope_label(), "workspace");
    }

    #[test]
    fn scope_filter_isolates_stores() {
        let ws = vec![anchor("alpha", "a", None, 0.0)];
        let gl = vec![anchor("beta", "b", None, 0.0)];
        assert_eq!(active_rows(&ws, &gl, ScopeFilter::Workspace, "").len(), 1);
        assert_eq!(
            active_rows(&ws, &gl, ScopeFilter::Global, "")[0]
                .anchor
                .identifier,
            "beta"
        );
    }

    #[test]
    fn query_matches_identifier_alias_description_domain() {
        let mut a = anchor("the_pipe", "msgpack framing", Some("Networking"), 0.0);
        a.aliases = vec!["wire format".to_owned()];
        let ws = vec![a];
        for q in ["PIPE", "msgpack", "wire", "network"] {
            assert_eq!(
                active_rows(&ws, &[], ScopeFilter::All, q).len(),
                1,
                "query {q}"
            );
        }
        assert!(active_rows(&ws, &[], ScopeFilter::All, "nope").is_empty());
    }

    fn row(id: &str, parent: Option<&str>) -> VocabRow {
        let mut a = anchor(id, id, None, 0.0);
        a.parent_anchor = parent.map(str::to_owned);
        VocabRow {
            anchor: a,
            scope: AnchorScopeTag::Workspace,
            stale: false,
        }
    }

    #[test]
    fn hierarchy_order_nests_children_under_parents() {
        // root -> child -> grandchild, plus a second root.
        let rows = vec![
            row("root", None),
            row("child", Some("root")),
            row("grandchild", Some("child")),
            row("other", None),
        ];
        let h = hierarchy_order(&rows);
        let seq: Vec<(&str, usize)> = h
            .iter()
            .map(|(r, d)| (r.anchor.identifier.as_str(), *d))
            .collect();
        assert_eq!(
            seq,
            vec![("root", 0), ("child", 1), ("grandchild", 2), ("other", 0)]
        );
    }

    #[test]
    fn hierarchy_order_treats_missing_parent_as_root() {
        let rows = vec![row("orphan", Some("not_present"))];
        let h = hierarchy_order(&rows);
        assert_eq!(h.len(), 1);
        assert_eq!(h[0].1, 0, "orphaned child is rendered as a root");
    }

    #[test]
    fn hierarchy_order_survives_a_cycle_and_emits_each_once() {
        // a -> b -> a (cycle); every row must still appear exactly once.
        let rows = vec![row("a", Some("b")), row("b", Some("a"))];
        let h = hierarchy_order(&rows);
        assert_eq!(h.len(), 2);
        let mut ids: Vec<&str> = h.iter().map(|(r, _)| r.anchor.identifier.as_str()).collect();
        ids.sort();
        assert_eq!(ids, vec!["a", "b"]);
    }

    #[test]
    fn views_partition_active_cleanup_stale_archived() {
        let now = 1_000_000_000.0;
        let fresh = anchor("fresh", "f", None, now - 100.0);
        let old = anchor("old_one", "o", None, now - CLEANUP_UNUSED_SECS - 1.0);
        let mut archived = anchor("shelved", "s", None, now - 100.0);
        archived.archived = true;
        let staleable = anchor("ghost_sym", "g", None, now - 50.0);
        let ws = vec![fresh, old, archived, staleable];
        let stale: HashSet<(AnchorScopeTag, String)> =
            [(AnchorScopeTag::Workspace, "ghost_sym".to_owned())]
                .into_iter()
                .collect();

        let active = rows(
            &ws,
            &[],
            ScopeFilter::All,
            ViewFilter::Active,
            "",
            now,
            &stale,
        );
        let ids: Vec<&str> = active
            .iter()
            .map(|r| r.anchor.identifier.as_str())
            .collect();
        assert_eq!(ids.len(), 3, "archived hidden from Active");
        assert!(!ids.contains(&"shelved"));
        // The stale badge rides along in every view.
        assert!(
            active
                .iter()
                .find(|r| r.anchor.identifier == "ghost_sym")
                .unwrap()
                .stale
        );

        let cleanup = rows(
            &ws,
            &[],
            ScopeFilter::All,
            ViewFilter::Cleanup,
            "",
            now,
            &stale,
        );
        assert_eq!(cleanup.len(), 1);
        assert_eq!(cleanup[0].anchor.identifier, "old_one");

        let stale_v = rows(
            &ws,
            &[],
            ScopeFilter::All,
            ViewFilter::Stale,
            "",
            now,
            &stale,
        );
        assert_eq!(stale_v.len(), 1);
        assert_eq!(stale_v[0].anchor.identifier, "ghost_sym");

        let arch = rows(
            &ws,
            &[],
            ScopeFilter::All,
            ViewFilter::Archived,
            "",
            now,
            &stale,
        );
        assert_eq!(arch.len(), 1);
        assert_eq!(arch[0].anchor.identifier, "shelved");
    }
}
