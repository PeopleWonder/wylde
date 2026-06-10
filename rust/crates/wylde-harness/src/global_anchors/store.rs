//! Global anchor persistence — the harness half of the anchor system.
//!
//! Global anchors are the cross-workspace vocabulary an anchor is **promoted**
//! into (Plan v2 §4.4). One flat JSON array lives at
//! `<data_dir>/global_anchors.json` (the harness `data_dir`, NOT under
//! `workspaces/`). Every record carries `scope = Global`.
//!
//! The store mirrors the per-workspace store in `wylde-workspaces` exactly —
//! same [`Anchor`] type (from `wylde-shared`, so the shapes are byte-identical
//! on both pipes), same atomic-write + owner-only harden discipline (OI-14),
//! same CRUD + the three lookups.
//!
//! ## Collision policy (OI-5)
//!
//! [`create`] is the promotion landing point. When `{{X}}` already exists
//! globally it does **not** overwrite — it returns
//! [`CreateOutcome::AlreadyExists`] with the existing record, which the verb
//! surfaces as an `already_exists_global` error carrying the existing
//! definition. The GUI Vocabulary tab renders the rename / keep-workspace-only
//! / replace dialog from that; the data layer never decides.

use std::path::PathBuf;

use wylde_shared::anchor::{validate_aliases, AliasError, Anchor, AnchorScope, AnchorTarget};

use crate::memory::common::data_dir;

/// `<data_dir>/global_anchors.json`.
pub fn global_anchors_path() -> PathBuf {
    data_dir().join("global_anchors.json")
}

/// Outcome of a [`create`]: a fresh record, a collision with the existing
/// global one (OI-5 — the caller shows the rename/keep/replace dialog; the
/// requested token is already taken as a global `identifier` **or** alias), or
/// an invalid alias set (Slice N-data-aliases).
#[derive(Clone, Debug, PartialEq)]
pub enum CreateOutcome {
    Created(Anchor),
    AlreadyExists(Anchor),
    AliasRejected(AliasError),
}

/// Outcome of an [`update`]: the patched record, no such identifier, or an
/// invalid alias set on the patch.
#[derive(Clone, Debug, PartialEq)]
pub enum UpdateOutcome {
    Updated(Anchor),
    NotFound,
    AliasRejected(AliasError),
}

/// Outcome of a [`replace`]: the stored record, no such identifier, or an
/// invalid alias set on the replacement.
#[derive(Clone, Debug, PartialEq)]
pub enum ReplaceOutcome {
    Replaced(Anchor),
    NotFound,
    AliasRejected(AliasError),
}

/// Load every global anchor. Fail-soft: empty on a missing/torn file.
pub fn load() -> Vec<Anchor> {
    let Ok(raw) = wylde_shared::encryption::read_to_string_at_rest(&global_anchors_path()) else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Encrypt-at-rest (OI-14) + atomically replace `global_anchors.json`, then
/// harden it to owner-only — all via the shared engine.
pub fn save(anchors: &[Anchor]) -> std::io::Result<()> {
    let body = serde_json::to_string_pretty(anchors).unwrap();
    wylde_shared::encryption::write_at_rest(&global_anchors_path(), body.as_bytes())
}

/// Create a global anchor. The stored record's scope is forced to `Global`. A
/// duplicate identifier returns [`CreateOutcome::AlreadyExists`] (the OI-5
/// collision) — never an overwrite.
pub fn create(mut anchor: Anchor) -> std::io::Result<CreateOutcome> {
    let mut all = load();
    if let Some(existing) = all.iter().find(|a| a.matches_token(&anchor.identifier)) {
        return Ok(CreateOutcome::AlreadyExists(existing.clone()));
    }
    match validate_aliases(&anchor.identifier, &anchor.aliases, &all) {
        Ok(normalized) => anchor.aliases = normalized,
        Err(e) => return Ok(CreateOutcome::AliasRejected(e)),
    }
    anchor.scope = AnchorScope::Global;
    all.push(anchor.clone());
    save(&all)?;
    Ok(CreateOutcome::Created(anchor))
}

/// Replace the definition of an existing global anchor (used by the collision
/// dialog's "Replace the global definition" branch, which requires explicit
/// user confirmation upstream). Forces `scope = Global`.
///
/// Validates the replacement's alias set against the *other* global anchors
/// before mutating — the same guard [`create`] applies — so a replacement that
/// would shadow another anchor's `identifier` or `alias` returns
/// [`ReplaceOutcome::AliasRejected`] and nothing is written. (Identifier
/// collision is structurally N/A here: `replace` targets the record that
/// already owns this `identifier`, and `validate_aliases` rejects any alias
/// that shadows another anchor's identifier, so the alias check subsumes the
/// alias-vs-identifier collision the create path guards.) An unknown identifier
/// returns [`ReplaceOutcome::NotFound`].
pub fn replace(mut anchor: Anchor) -> std::io::Result<ReplaceOutcome> {
    let mut all = load();
    let Some(idx) = all.iter().position(|a| a.identifier == anchor.identifier) else {
        return Ok(ReplaceOutcome::NotFound);
    };
    match validate_aliases(&anchor.identifier, &anchor.aliases, &all) {
        Ok(normalized) => anchor.aliases = normalized,
        Err(e) => return Ok(ReplaceOutcome::AliasRejected(e)),
    }
    anchor.scope = AnchorScope::Global;
    all[idx] = anchor.clone();
    save(&all)?;
    Ok(ReplaceOutcome::Replaced(anchor))
}

/// Fields a caller may patch on an existing global anchor (mirrors the
/// workspace store's `AnchorPatch`).
#[derive(Clone, Debug, Default)]
pub struct AnchorPatch {
    pub description: Option<String>,
    pub target: Option<AnchorTarget>,
    /// Wholesale-replace the alias list (Slice N-data-aliases); `None` leaves it.
    pub aliases: Option<Vec<String>>,
    pub related_to: Option<Vec<String>>,
    pub parent_anchor: Option<Option<String>>,
    pub domain: Option<Option<String>>,
    /// Archive / unarchive (OI-21 Recommended Cleanup, Slice N).
    pub archived: Option<bool>,
}

/// Patch a global anchor by identifier. Re-stamps `last_used_at`. A patched
/// alias set is normalised + validated against the *other* global anchors
/// before any mutation, so a bad set returns [`UpdateOutcome::AliasRejected`]
/// and nothing is written; an unknown identifier returns
/// [`UpdateOutcome::NotFound`].
pub fn update(identifier: &str, patch: AnchorPatch) -> std::io::Result<UpdateOutcome> {
    let mut all = load();
    let Some(idx) = all.iter().position(|a| a.identifier == identifier) else {
        return Ok(UpdateOutcome::NotFound);
    };
    let new_aliases = match patch.aliases {
        Some(raw) => match validate_aliases(identifier, &raw, &all) {
            Ok(normalized) => Some(normalized),
            Err(e) => return Ok(UpdateOutcome::AliasRejected(e)),
        },
        None => None,
    };
    {
        let a = &mut all[idx];
        if let Some(d) = patch.description {
            a.description = d;
        }
        if let Some(t) = patch.target {
            a.target = t;
        }
        if let Some(aliases) = new_aliases {
            a.aliases = aliases;
        }
        if let Some(r) = patch.related_to {
            a.related_to = r;
        }
        if let Some(p) = patch.parent_anchor {
            a.parent_anchor = p;
        }
        if let Some(dom) = patch.domain {
            a.domain = dom;
        }
        if let Some(arch) = patch.archived {
            a.archived = arch;
        }
        a.last_used_at = wylde_shared::anchor::epoch_now();
    }
    let updated = all[idx].clone();
    save(&all)?;
    Ok(UpdateOutcome::Updated(updated))
}

/// Remove a global anchor by identifier. `true` iff one was removed.
pub fn delete(identifier: &str) -> std::io::Result<bool> {
    let all = load();
    let before = all.len();
    let kept: Vec<Anchor> = all
        .into_iter()
        .filter(|a| a.identifier != identifier)
        .collect();
    if kept.len() == before {
        return Ok(false);
    }
    save(&kept)?;
    Ok(true)
}

/// Global anchors matching `token` against the canonical `identifier` **or** an
/// `alias` (0 or 1 — tokens are unique globally). `token` must be
/// whitespace-normalised
/// ([`wylde_shared::anchor_tokenizer::normalize_lookup_token`]).
pub fn find_by_token(token: &str) -> Vec<Anchor> {
    load()
        .into_iter()
        // Archived anchors stop resolving (OI-21, Slice N) — recoverable
        // from the Vocabulary tab, never silently decayed.
        .filter(|a| !a.archived && a.matches_token(token))
        .collect()
}

/// Global anchors targeting `symbol_id` (inverse lookup, OI-20).
pub fn find_by_target(symbol_id: &str) -> Vec<Anchor> {
    load()
        .into_iter()
        .filter(|a| a.target.symbol_id() == Some(symbol_id))
        .collect()
}

/// Global anchors under a taxonomy parent (hierarchy, OI-19).
pub fn list_under(parent_id: &str) -> Vec<Anchor> {
    load()
        .into_iter()
        .filter(|a| a.parent_anchor.as_deref() == Some(parent_id))
        .collect()
}

/// Global anchors unused for more than `max_idle_secs` — the "Recommended
/// Cleanup" surface (OI-21). **No auto-archive**: this only *lists* stale
/// anchors for the Vocabulary tab to offer manual archive/edit/keep. `now` is
/// epoch seconds.
pub fn recommended_cleanup(now: f64, max_idle_secs: f64) -> Vec<Anchor> {
    load()
        .into_iter()
        .filter(|a| now - a.last_used_at > max_idle_secs)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;
    use tempfile::TempDir;
    use wylde_shared::anchor::{Anchor, AnchorKind, AnchorScope, AnchorTarget};

    /// Per-test `WYLDE_DATA_DIR` sandbox, sharing the process-wide env mutex
    /// every harness store test uses.
    struct Env {
        _g: MutexGuard<'static, ()>,
        _td: TempDir,
        prior: Option<std::ffi::OsString>,
    }
    impl Env {
        fn new() -> Self {
            let g = crate::memory::common::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let td = TempDir::new().unwrap();
            let prior = std::env::var_os("WYLDE_DATA_DIR");
            std::env::set_var("WYLDE_DATA_DIR", td.path());
            Self {
                _g: g,
                _td: td,
                prior,
            }
        }
    }
    impl Drop for Env {
        fn drop(&mut self) {
            match self.prior.take() {
                Some(v) => std::env::set_var("WYLDE_DATA_DIR", v),
                None => std::env::remove_var("WYLDE_DATA_DIR"),
            }
        }
    }

    fn global_concept(id: &str) -> Anchor {
        Anchor::new(
            id,
            AnchorKind::Concept,
            AnchorTarget::Concept {
                text: format!("def {id}"),
            },
            AnchorScope::Global,
            format!("desc {id}"),
        )
    }

    #[test]
    fn create_then_load_round_trips() {
        let _e = Env::new();
        let out = create(global_concept("g_alpha")).unwrap();
        assert!(matches!(out, CreateOutcome::Created(_)));
        let back = load();
        assert_eq!(back.len(), 1);
        assert!(back[0].scope.is_global());
    }

    #[test]
    fn create_collision_returns_already_exists() {
        let _e = Env::new();
        create(global_concept("dup")).unwrap();
        match create(global_concept("dup")).unwrap() {
            CreateOutcome::AlreadyExists(a) => {
                assert_eq!(a.identifier, "dup");
                assert_eq!(a.description, "desc dup");
            }
            _ => panic!("expected AlreadyExists collision (OI-5)"),
        }
        assert_eq!(load().len(), 1, "no overwrite");
    }

    #[test]
    fn create_forces_global_scope() {
        let _e = Env::new();
        let mut a = global_concept("x");
        a.scope = AnchorScope::Workspace {
            workspace_id: "leak".into(),
        };
        let CreateOutcome::Created(stored) = create(a).unwrap() else {
            panic!("created");
        };
        assert!(stored.scope.is_global());
    }

    #[test]
    fn replace_swaps_definition() {
        let _e = Env::new();
        create(global_concept("r")).unwrap();
        let mut updated = global_concept("r");
        updated.description = "replaced".into();
        let out = match replace(updated).unwrap() {
            ReplaceOutcome::Replaced(a) => a,
            other => panic!("expected Replaced, got {other:?}"),
        };
        assert_eq!(out.description, "replaced");
        assert_eq!(load()[0].description, "replaced");
        // Replacing an absent id is NotFound.
        assert!(matches!(
            replace(global_concept("ghost")).unwrap(),
            ReplaceOutcome::NotFound
        ));
    }

    #[test]
    fn replace_rejects_conflicting_alias() {
        let _e = Env::new();
        // Two distinct global anchors exist.
        create(global_concept("first")).unwrap();
        create(global_concept("second")).unwrap();
        // Replacing "second" with an alias that shadows "first"'s identifier is
        // rejected with the structured collision error — same guard as create.
        let mut clash = global_concept("second");
        clash.aliases = vec!["first".into()];
        match replace(clash).unwrap() {
            ReplaceOutcome::AliasRejected(AliasError::Collision { owned_by, .. }) => {
                assert_eq!(owned_by, "first")
            }
            other => panic!("expected AliasRejected, got {other:?}"),
        }
        // Nothing was written — "second" keeps its original (empty) alias set.
        let stored = find_by_token("second");
        assert_eq!(stored.len(), 1);
        assert!(stored[0].aliases.is_empty());
    }

    #[test]
    fn replace_keeps_own_aliases() {
        // A replacement may retain/normalise its own aliases (the record being
        // replaced is skipped by the self-collision guard).
        let _e = Env::new();
        let mut orig = global_concept("widget");
        orig.aliases = vec!["the widget".into()];
        create(orig).unwrap();
        let mut updated = global_concept("widget");
        updated.aliases = vec!["  the   widget ".into()]; // same alias, messy ws
        updated.description = "v2".into();
        match replace(updated).unwrap() {
            ReplaceOutcome::Replaced(a) => {
                assert_eq!(a.description, "v2");
                assert_eq!(a.aliases, vec!["the widget"], "normalised, not rejected");
            }
            other => panic!("expected Replaced, got {other:?}"),
        }
    }

    #[test]
    fn update_delete_and_lookups() {
        let _e = Env::new();
        let mut sym = Anchor::new(
            "the_fn",
            AnchorKind::CodeSymbol,
            AnchorTarget::CodeSymbol {
                symbol_id: "run".into(),
            },
            AnchorScope::Global,
            "runs",
        );
        sym.parent_anchor = Some("infra".into());
        create(sym).unwrap();
        create(global_concept("infra")).unwrap();

        let patched = match update(
            "the_fn",
            AnchorPatch {
                description: Some("now runs faster".into()),
                ..Default::default()
            },
        )
        .unwrap()
        {
            UpdateOutcome::Updated(a) => a,
            other => panic!("expected Updated, got {other:?}"),
        };
        assert_eq!(patched.description, "now runs faster");

        assert_eq!(find_by_token("the_fn").len(), 1);
        assert_eq!(find_by_target("run").len(), 1);
        assert_eq!(list_under("infra").len(), 1);

        assert!(delete("the_fn").unwrap());
        assert!(find_by_token("the_fn").is_empty());
        assert!(!delete("the_fn").unwrap());
    }

    #[test]
    fn create_with_aliases_resolves_via_find_by_token() {
        let _e = Env::new();
        let mut a = global_concept("set_active_graph_view");
        a.aliases = vec!["  set   active ".into()];
        let CreateOutcome::Created(stored) = create(a).unwrap() else {
            panic!("created");
        };
        assert_eq!(stored.aliases, vec!["set active"], "normalised on write");
        // Resolve via the alias → canonical anchor returned.
        let hits = find_by_token("set active");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].identifier, "set_active_graph_view");
    }

    #[test]
    fn create_rejects_alias_collision_globally() {
        let _e = Env::new();
        create(global_concept("first")).unwrap();
        let mut second = global_concept("second");
        second.aliases = vec!["first".into()]; // collides with another identifier
        match create(second).unwrap() {
            CreateOutcome::AliasRejected(AliasError::Collision { owned_by, .. }) => {
                assert_eq!(owned_by, "first")
            }
            other => panic!("expected AliasRejected, got {other:?}"),
        }
    }

    #[test]
    fn recommended_cleanup_lists_stale_only() {
        let _e = Env::new();
        let mut old = global_concept("ancient");
        old.last_used_at = 0.0;
        let mut fresh = global_concept("fresh");
        fresh.last_used_at = 1_000_000.0;
        save(&[old, fresh]).unwrap();
        let stale = recommended_cleanup(1_000_000.0, 100.0);
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].identifier, "ancient");
    }
}
