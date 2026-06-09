//! Per-workspace anchor persistence.
//!
//! Each workspace owns one `anchors.json` — a JSON **array** of [`Anchor`] —
//! at `<data_dir>/workspaces/<workspace_id>/anchors.json`, alongside the
//! workspace's `definition.json` / `persona.md` / `memory.jsonl`. A single
//! flat array (rather than JSONL) because the anchor set is small, frequently
//! re-read whole (the vocabulary block, the Vocabulary tab) and edited
//! by-identifier.
//!
//! Atomic-write discipline matches the rest of the store layer: reads tolerate
//! a torn/missing file by returning an empty vec, writes go to `<path>.tmp`
//! then rename. After each write the file is hardened to owner-only
//! ([`wylde_shared::secure_file::harden_perms`]) — the file-level protection
//! the codebase uses for sensitive state (OI-14; see the module-level note in
//! [`super`] on the full encryption-at-rest follow-up).
//!
//! `identifier` is unique within a workspace store: [`create`] refuses a
//! duplicate (returning [`CreateOutcome::AlreadyExists`]) rather than minting a
//! second record under the same token.

use std::path::PathBuf;


use super::anchor::{validate_aliases, AliasError, Anchor, AnchorScope, AnchorTarget};
use crate::registry::persistence::workspace_dir;

/// `<data_dir>/workspaces/<workspace_id>/anchors.json`.
pub fn anchors_path(workspace_id: &str) -> PathBuf {
    workspace_dir(workspace_id).join("anchors.json")
}

/// Outcome of a [`create`]: a fresh record, a collision with an existing one
/// (the requested `identifier` is already taken in this workspace — as an
/// identifier **or** as another anchor's alias), or an invalid alias set.
#[derive(Clone, Debug, PartialEq)]
pub enum CreateOutcome {
    Created(Anchor),
    AlreadyExists(Anchor),
    /// One of the candidate `aliases` failed validation (Slice N-data-aliases).
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

/// Load every anchor for a workspace. Fail-soft: empty on a missing/torn file.
/// Decrypts at rest (OI-14).
pub fn load(workspace_id: &str) -> Vec<Anchor> {
    let Ok(raw) = wylde_shared::encryption::read_to_string_at_rest(&anchors_path(workspace_id))
    else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Encrypt-at-rest (OI-14) + atomically replace a workspace's `anchors.json`,
/// then harden it to owner-only — all via the shared engine.
pub fn save(workspace_id: &str, anchors: &[Anchor]) -> std::io::Result<()> {
    let path = workspace_dir(workspace_id).join("anchors.json");
    let body = serde_json::to_string_pretty(anchors).unwrap();
    wylde_shared::encryption::write_at_rest(&path, body.as_bytes())
}

/// Insert a new anchor. Refuses a duplicate token in this workspace — the
/// requested `identifier` colliding with an existing anchor's `identifier`
/// **or** one of its `aliases` returns [`CreateOutcome::AlreadyExists`] (a
/// lookup must stay unambiguous). The candidate `aliases` are normalised +
/// validated ([`validate_aliases`]); a bad set returns
/// [`CreateOutcome::AliasRejected`]. The stored anchor's scope is forced to
/// `Workspace(workspace_id)` so a mis-scoped caller can't poison the store.
pub fn create(workspace_id: &str, mut anchor: Anchor) -> std::io::Result<CreateOutcome> {
    let mut all = load(workspace_id);
    if let Some(existing) = all.iter().find(|a| a.matches_token(&anchor.identifier)) {
        return Ok(CreateOutcome::AlreadyExists(existing.clone()));
    }
    match validate_aliases(&anchor.identifier, &anchor.aliases, &all) {
        Ok(normalized) => anchor.aliases = normalized,
        Err(e) => return Ok(CreateOutcome::AliasRejected(e)),
    }
    anchor.scope = AnchorScope::Workspace {
        workspace_id: workspace_id.to_owned(),
    };
    all.push(anchor.clone());
    save(workspace_id, &all)?;
    Ok(CreateOutcome::Created(anchor))
}

/// Fields a caller may patch on an existing anchor. `None` leaves the field
/// untouched; `Some` replaces it. (`related_to`/`parent_anchor`/`domain` are
/// set wholesale — the peer-to-peer/hierarchy editors compute the new list.)
#[derive(Clone, Debug, Default)]
pub struct AnchorPatch {
    pub description: Option<String>,
    pub target: Option<AnchorTarget>,
    /// Wholesale-replace the alias list (Slice N-data-aliases). `None` leaves
    /// the existing aliases untouched; `Some(vec)` re-validates the new set.
    pub aliases: Option<Vec<String>>,
    pub related_to: Option<Vec<String>>,
    pub parent_anchor: Option<Option<String>>,
    pub domain: Option<Option<String>>,
}

/// Apply a patch to the anchor with `identifier`. Re-stamps `last_used_at`.
/// A patched alias set is normalised + validated against the *other* anchors in
/// the workspace ([`validate_aliases`] skips the anchor being edited), so an
/// update can re-save the same aliases without tripping on itself; an invalid
/// set returns [`UpdateOutcome::AliasRejected`] and nothing is written.
pub fn update(
    workspace_id: &str,
    identifier: &str,
    patch: AnchorPatch,
) -> std::io::Result<UpdateOutcome> {
    let mut all = load(workspace_id);
    let Some(idx) = all.iter().position(|a| a.identifier == identifier) else {
        return Ok(UpdateOutcome::NotFound);
    };
    // Validate a patched alias set against the rest of the store *before*
    // mutating, so a rejected patch leaves the store untouched.
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
        a.last_used_at = super::anchor::epoch_now();
    }
    let updated = all[idx].clone();
    save(workspace_id, &all)?;
    Ok(UpdateOutcome::Updated(updated))
}

/// Remove the anchor with `identifier`. Returns `true` iff one was removed.
pub fn delete(workspace_id: &str, identifier: &str) -> std::io::Result<bool> {
    let all = load(workspace_id);
    let before = all.len();
    let kept: Vec<Anchor> = all
        .into_iter()
        .filter(|a| a.identifier != identifier)
        .collect();
    if kept.len() == before {
        return Ok(false);
    }
    save(workspace_id, &kept)?;
    Ok(true)
}

/// Every anchor whose `identifier` **or one of its `aliases`** matches `token`
/// (the `{{token}}` resolver). `token` must already be whitespace-normalised
/// ([`wylde_shared::anchor_tokenizer::normalize_lookup_token`]) so an alias with
/// spaces compares correctly. Tokens are unique across identifiers+aliases in a
/// store (enforced by [`create`]/[`update`]), so this returns 0 or 1 — the
/// `Vec` shape lets the composer merge workspace + global hits uniformly.
pub fn find_by_token(workspace_id: &str, token: &str) -> Vec<Anchor> {
    load(workspace_id)
        .into_iter()
        .filter(|a| a.matches_token(token))
        .collect()
}

/// Look up an anchor by its `identifier` for promotion — the data half of the
/// `promote_via_alias` verb (Slice N-data-aliases). Returns the full record
/// (carrying **all** its aliases) so the caller can hand it to the global
/// `anchors.create` landing point; promotion of an alias promotes the *whole*
/// anchor. `None` if no anchor has that identifier.
pub fn get(workspace_id: &str, identifier: &str) -> Option<Anchor> {
    load(workspace_id)
        .into_iter()
        .find(|a| a.identifier == identifier)
}

/// Every anchor targeting `symbol_id` (the inverse lookup, OI-20).
pub fn find_by_target(workspace_id: &str, symbol_id: &str) -> Vec<Anchor> {
    load(workspace_id)
        .into_iter()
        .filter(|a| a.target.symbol_id() == Some(symbol_id))
        .collect()
}

/// Every anchor whose `parent_anchor` is `parent_id` (hierarchy traversal,
/// OI-19).
pub fn list_under(workspace_id: &str, parent_id: &str) -> Vec<Anchor> {
    load(workspace_id)
        .into_iter()
        .filter(|a| a.parent_anchor.as_deref() == Some(parent_id))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::anchors::anchor::{workspace_anchor, AnchorKind};
    use crate::test_support::TestEnv;

    fn concept(ws: &str, id: &str) -> Anchor {
        workspace_anchor(
            ws,
            id,
            AnchorKind::Concept,
            AnchorTarget::Concept {
                text: format!("def of {id}"),
            },
            format!("desc {id}"),
        )
    }

    #[test]
    fn create_then_load_round_trips() {
        let _env = TestEnv::new();
        let ws = "ws-anchor-000000";
        let out = create(ws, concept(ws, "alpha")).unwrap();
        assert!(matches!(out, CreateOutcome::Created(_)));
        let back = load(ws);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].identifier, "alpha");
        assert_eq!(back[0].scope.workspace_id(), Some(ws));
    }

    #[test]
    fn create_rejects_duplicate_identifier() {
        let _env = TestEnv::new();
        let ws = "ws-dup-000000";
        create(ws, concept(ws, "same")).unwrap();
        let out = create(ws, concept(ws, "same")).unwrap();
        match out {
            CreateOutcome::AlreadyExists(a) => assert_eq!(a.identifier, "same"),
            _ => panic!("expected AlreadyExists"),
        }
        assert_eq!(load(ws).len(), 1, "no second record minted");
    }

    #[test]
    fn create_forces_workspace_scope() {
        let _env = TestEnv::new();
        let ws = "ws-scope-000000";
        // A caller hands in a Global-scoped anchor; the store must override.
        let mut a = concept(ws, "x");
        a.scope = AnchorScope::Global;
        let out = create(ws, a).unwrap();
        let CreateOutcome::Created(stored) = out else {
            panic!("created");
        };
        assert_eq!(stored.scope.workspace_id(), Some(ws));
    }

    #[test]
    fn update_patches_and_restamps() {
        let _env = TestEnv::new();
        let ws = "ws-upd-000000";
        create(ws, concept(ws, "edit_me")).unwrap();
        let patched = match update(
            ws,
            "edit_me",
            AnchorPatch {
                description: Some("new desc".into()),
                domain: Some(Some("Storage".into())),
                ..Default::default()
            },
        )
        .unwrap()
        {
            UpdateOutcome::Updated(a) => a,
            other => panic!("expected Updated, got {other:?}"),
        };
        assert_eq!(patched.description, "new desc");
        assert_eq!(patched.domain.as_deref(), Some("Storage"));
        assert_eq!(load(ws)[0].description, "new desc");
        // Unknown id → NotFound.
        assert_eq!(
            update(ws, "ghost", AnchorPatch::default()).unwrap(),
            UpdateOutcome::NotFound
        );
    }

    #[test]
    fn create_stores_and_normalizes_aliases() {
        let _env = TestEnv::new();
        let ws = "ws-alias-000000";
        let mut a = concept(ws, "set_active_graph_view");
        a.aliases = vec!["  set   active ".into(), "graph view".into()];
        let CreateOutcome::Created(stored) = create(ws, a).unwrap() else {
            panic!("created");
        };
        assert_eq!(stored.aliases, vec!["set active", "graph view"]);
        assert_eq!(load(ws)[0].aliases, vec!["set active", "graph view"]);
    }

    #[test]
    fn find_by_token_resolves_aliases() {
        let _env = TestEnv::new();
        let ws = "ws-alias-tok-0000";
        let mut a = concept(ws, "set_active_graph_view");
        a.aliases = vec!["set active".into()];
        create(ws, a).unwrap();

        // Canonical identifier resolves.
        assert_eq!(find_by_token(ws, "set_active_graph_view").len(), 1);
        // Alias (whitespace-normalised) resolves to the same canonical anchor.
        let hits = find_by_token(ws, "set active");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].identifier, "set_active_graph_view");
        assert!(find_by_token(ws, "nope").is_empty());
    }

    #[test]
    fn create_rejects_identifier_colliding_with_existing_alias() {
        let _env = TestEnv::new();
        let ws = "ws-alias-coll-000";
        let mut a = concept(ws, "anchor_one");
        a.aliases = vec!["shared name".into()];
        create(ws, a).unwrap();

        // A new anchor whose IDENTIFIER equals an existing alias is rejected
        // (find_by_token would otherwise be ambiguous).
        let b = concept(ws, "shared name"); // (identifier validity is an API concern; the store guards collisions)
        match create(ws, b).unwrap() {
            CreateOutcome::AlreadyExists(existing) => {
                assert_eq!(existing.identifier, "anchor_one")
            }
            other => panic!("expected AlreadyExists, got {other:?}"),
        }
    }

    #[test]
    fn create_rejects_alias_colliding_with_other_anchor() {
        let _env = TestEnv::new();
        let ws = "ws-alias-coll2-00";
        create(ws, concept(ws, "existing_anchor")).unwrap();
        let mut b = concept(ws, "new_anchor");
        b.aliases = vec!["existing_anchor".into()]; // collides with another identifier
        match create(ws, b).unwrap() {
            CreateOutcome::AliasRejected(AliasError::Collision { owned_by, .. }) => {
                assert_eq!(owned_by, "existing_anchor")
            }
            other => panic!("expected AliasRejected(Collision), got {other:?}"),
        }
    }

    #[test]
    fn update_can_resave_own_aliases_and_validates_new_ones() {
        let _env = TestEnv::new();
        let ws = "ws-alias-upd-000";
        let mut a = concept(ws, "mine");
        a.aliases = vec!["my alias".into()];
        create(ws, a).unwrap();
        create(ws, concept(ws, "other")).unwrap();

        // Re-saving the same alias plus a new one is fine (self is skipped).
        let out = update(
            ws,
            "mine",
            AnchorPatch {
                aliases: Some(vec!["my alias".into(), "second".into()]),
                ..Default::default()
            },
        )
        .unwrap();
        let UpdateOutcome::Updated(a) = out else {
            panic!("expected Updated, got {out:?}");
        };
        assert_eq!(a.aliases, vec!["my alias", "second"]);

        // Patching an alias that collides with another anchor's identifier fails.
        let rejected = update(
            ws,
            "mine",
            AnchorPatch {
                aliases: Some(vec!["other".into()]),
                ..Default::default()
            },
        )
        .unwrap();
        assert!(matches!(
            rejected,
            UpdateOutcome::AliasRejected(AliasError::Collision { .. })
        ));
    }

    #[test]
    fn delete_removes_and_reports() {
        let _env = TestEnv::new();
        let ws = "ws-del-000000";
        create(ws, concept(ws, "a")).unwrap();
        create(ws, concept(ws, "b")).unwrap();
        assert!(delete(ws, "a").unwrap());
        assert_eq!(load(ws).len(), 1);
        assert!(!delete(ws, "a").unwrap(), "second delete is a no-op");
    }

    #[test]
    fn find_by_token_matches_identifier() {
        let _env = TestEnv::new();
        let ws = "ws-tok-000000";
        create(ws, concept(ws, "wanted")).unwrap();
        create(ws, concept(ws, "other")).unwrap();
        let hits = find_by_token(ws, "wanted");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].identifier, "wanted");
        assert!(find_by_token(ws, "nope").is_empty());
    }

    #[test]
    fn find_by_target_does_inverse_lookup() {
        let _env = TestEnv::new();
        let ws = "ws-inv-000000";
        let sym = workspace_anchor(
            ws,
            "the_fn",
            AnchorKind::CodeSymbol,
            AnchorTarget::CodeSymbol {
                symbol_id: "run_pipeline".into(),
            },
            "runs it",
        );
        create(ws, sym).unwrap();
        create(ws, concept(ws, "unrelated")).unwrap();
        let hits = find_by_target(ws, "run_pipeline");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].identifier, "the_fn");
        assert!(find_by_target(ws, "missing_symbol").is_empty());
    }

    #[test]
    fn list_under_traverses_hierarchy() {
        let _env = TestEnv::new();
        let ws = "ws-hier-000000";
        create(ws, concept(ws, "migration_pattern")).unwrap();
        let mut child = concept(ws, "strangler_fig_pattern");
        child.parent_anchor = Some("migration_pattern".into());
        create(ws, child).unwrap();
        let kids = list_under(ws, "migration_pattern");
        assert_eq!(kids.len(), 1);
        assert_eq!(kids[0].identifier, "strangler_fig_pattern");
        assert!(list_under(ws, "strangler_fig_pattern").is_empty());
    }

    #[test]
    fn load_is_empty_for_missing_file() {
        let _env = TestEnv::new();
        assert!(load("nope-000000").is_empty());
    }
}
