//! Vocabulary list model (Slice N, Build Order §4 `vocabulary/list_view`):
//! merge the workspace + global stores into filterable, sortable rows. Pure
//! and gpui-free; the tab renders [`VocabRow`]s and routes clicks.

use super::ipc::{AnchorScopeTag, AnchorView};

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

/// One list row: the anchor + which store it came from.
#[derive(Clone, Debug, PartialEq)]
pub struct VocabRow {
    pub anchor: AnchorView,
    pub scope: AnchorScopeTag,
}

impl VocabRow {
    pub fn scope_label(&self) -> &'static str {
        match self.scope {
            AnchorScopeTag::Workspace => "workspace",
            AnchorScopeTag::Global => "global",
        }
    }
}

/// Build the visible rows: merge both stores, apply the scope filter and the
/// free-text query (matched against identifier, aliases, description and
/// domain, case-insensitive), then sort by `last_used_at` (desc — the living
/// vocabulary floats up) with identifier as the deterministic tiebreak.
pub fn rows(
    workspace: &[AnchorView],
    global: &[AnchorView],
    filter: ScopeFilter,
    query: &str,
) -> Vec<VocabRow> {
    let q = query.trim().to_lowercase();
    let mut out: Vec<VocabRow> = Vec::new();

    if filter != ScopeFilter::Global {
        out.extend(workspace.iter().cloned().map(|anchor| VocabRow {
            anchor,
            scope: AnchorScopeTag::Workspace,
        }));
    }
    if filter != ScopeFilter::Workspace {
        out.extend(global.iter().cloned().map(|anchor| VocabRow {
            anchor,
            scope: AnchorScopeTag::Global,
        }));
    }

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

    #[test]
    fn merges_scopes_and_sorts_recent_first() {
        let ws = vec![anchor("alpha", "a", None, 100.0)];
        let gl = vec![
            anchor("beta", "b", None, 300.0),
            anchor("gamma", "c", None, 100.0),
        ];
        let r = rows(&ws, &gl, ScopeFilter::All, "");
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
        assert_eq!(rows(&ws, &gl, ScopeFilter::Workspace, "").len(), 1);
        assert_eq!(
            rows(&ws, &gl, ScopeFilter::Global, "")[0].anchor.identifier,
            "beta"
        );
    }

    #[test]
    fn query_matches_identifier_alias_description_domain() {
        let mut a = anchor("the_pipe", "msgpack framing", Some("Networking"), 0.0);
        a.aliases = vec!["wire format".to_owned()];
        let ws = vec![a];
        for q in ["PIPE", "msgpack", "wire", "network"] {
            assert_eq!(rows(&ws, &[], ScopeFilter::All, q).len(), 1, "query {q}");
        }
        assert!(rows(&ws, &[], ScopeFilter::All, "nope").is_empty());
    }
}
