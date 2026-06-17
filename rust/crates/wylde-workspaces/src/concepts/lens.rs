//! Concept-as-scoped-lens (TBS concept-system Phase 3, thesis §3.1) —
//! `lens(concept, scope) = { n ∈ concept.MEMBER : n.file ∈ region(scope) }`.
//!
//! A concept is a *node set*; a scope is a *region of the graph* (repo →
//! service → file → extension), expressed as a path-subtree prefix. Seeing a
//! concept "within" a scope is the set intersection — O(|members|), no
//! embedding work. This **composes with the workspace scoping already built**
//! (the active-file/region plumbing slice 2.5 threads): a concept lens is the
//! same idea one level up — "this concept, restricted to this subsystem."
//!
//! Pure + gpui-free; the intersection is a path-prefix filter on the concept's
//! `member_files`.

/// True iff `file` lies within `scope` — an exact match or a path-component-
/// boundary descendant (handles `/` and `\`). An empty scope matches nothing
/// here (callers pass `None` for "no scope" / whole concept).
pub fn in_region(file: &str, scope: &str) -> bool {
    if scope.is_empty() {
        return false;
    }
    let f = file.replace('\\', "/");
    let s = scope.replace('\\', "/").trim_end_matches('/').to_owned();
    if f == s {
        return true;
    }
    match f.strip_prefix(&s) {
        Some(rest) => rest.starts_with('/'),
        None => false,
    }
}

/// Intersect a concept's member files with a scope region. `scope = None` (or
/// blank) returns the whole member set (the concept seen at repo scope).
pub fn lens<'a>(member_files: &'a [String], scope: Option<&str>) -> Vec<&'a String> {
    match scope.map(str::trim).filter(|s| !s.is_empty()) {
        None => member_files.iter().collect(),
        Some(s) => member_files.iter().filter(|f| in_region(f, s)).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn in_region_respects_component_boundaries() {
        assert!(in_region("services/vpn/tunnel.rs", "services/vpn"));
        assert!(in_region("services/vpn/tunnel.rs", "services/vpn/"));
        assert!(in_region("services\\vpn\\tunnel.rs", "services/vpn"));
        assert!(in_region("services/vpn", "services/vpn")); // exact
        // prefix-but-not-a-boundary must not match.
        assert!(!in_region("services/vpnx/a.rs", "services/vpn"));
        assert!(!in_region("services/auth/a.rs", "services/vpn"));
        assert!(!in_region("any.rs", ""));
    }

    #[test]
    fn lens_without_scope_returns_all() {
        let files = vec!["a/x.rs".to_owned(), "b/y.rs".to_owned()];
        assert_eq!(lens(&files, None).len(), 2);
        assert_eq!(lens(&files, Some("  ")).len(), 2, "blank scope = no scope");
    }

    #[test]
    fn lens_intersects_with_scope() {
        let files = vec![
            "services/vpn/a.rs".to_owned(),
            "services/vpn/sub/b.rs".to_owned(),
            "services/auth/c.rs".to_owned(),
        ];
        let scoped: Vec<&String> = lens(&files, Some("services/vpn"));
        assert_eq!(scoped.len(), 2, "only the vpn subtree");
        assert!(scoped.iter().all(|f| f.contains("/vpn/")));
    }
}
