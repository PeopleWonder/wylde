//! Per-workspace concept persistence — `concepts.json`.
//!
//! Mirrors [`crate::anchors::store`] exactly: one JSON **array** of [`Concept`]
//! at `<data_dir>/workspaces/<workspace_id>/concepts.json`, encrypted at rest
//! (OI-14), atomic-write, fail-soft reads. A flat array (not JSONL) because the
//! concept set is small (~100–200), re-read whole by the Concepts sub-tab, and
//! edited by-id.
//!
//! `id` is unique within a workspace store: [`upsert`] replaces an existing
//! record with the same id (the build pass re-runs idempotently); [`create`]
//! refuses a duplicate.
//!
//! The reverse-lookup queries ([`find_by_member`], [`find_by_file`]) are the
//! pure-store realisation of the thesis §4.2 "from this symbol → its concepts"
//! traversal — no Neo4j needed, the member sets live right here.

use std::path::PathBuf;

use super::concept::Concept;
use crate::registry::persistence::workspace_dir;

/// `<data_dir>/workspaces/<workspace_id>/concepts.json`.
pub fn concepts_path(workspace_id: &str) -> PathBuf {
    workspace_dir(workspace_id).join("concepts.json")
}

/// Load every concept for a workspace. Fail-soft: empty on a missing/torn file.
/// Decrypts at rest (OI-14).
pub fn load(workspace_id: &str) -> Vec<Concept> {
    let Ok(raw) = wylde_shared::encryption::read_to_string_at_rest(&concepts_path(workspace_id))
    else {
        return Vec::new();
    };
    serde_json::from_str(&raw).unwrap_or_default()
}

/// Encrypt-at-rest (OI-14) + atomically replace `concepts.json`.
pub fn save(workspace_id: &str, concepts: &[Concept]) -> std::io::Result<()> {
    let body = serde_json::to_string_pretty(concepts).unwrap();
    wylde_shared::encryption::write_at_rest(&concepts_path(workspace_id), body.as_bytes())
}

/// Outcome of a [`create`].
#[derive(Clone, Debug, PartialEq)]
pub enum CreateOutcome {
    Created(Concept),
    AlreadyExists(Concept),
}

/// Insert a new concept, refusing a duplicate id.
pub fn create(workspace_id: &str, concept: Concept) -> std::io::Result<CreateOutcome> {
    let mut all = load(workspace_id);
    if let Some(existing) = all.iter().find(|c| c.id == concept.id) {
        return Ok(CreateOutcome::AlreadyExists(existing.clone()));
    }
    all.push(concept.clone());
    save(workspace_id, &all)?;
    Ok(CreateOutcome::Created(concept))
}

/// Insert-or-replace by id, re-stamping `updated_at`. The build pass uses this
/// so a re-run replaces a directory concept in place rather than colliding.
/// `created_at` is preserved from the prior record when one exists.
pub fn upsert(workspace_id: &str, mut concept: Concept) -> std::io::Result<Concept> {
    let mut all = load(workspace_id);
    concept.updated_at = wylde_shared::anchor::epoch_now();
    match all.iter().position(|c| c.id == concept.id) {
        Some(idx) => {
            concept.created_at = all[idx].created_at;
            all[idx] = concept.clone();
        }
        None => all.push(concept.clone()),
    }
    save(workspace_id, &all)?;
    Ok(concept)
}

/// Replace the whole concept set in one write (the build pass produces the full
/// set, so a single atomic swap beats N upserts). Stamps `updated_at` on each.
pub fn replace_all(workspace_id: &str, mut concepts: Vec<Concept>) -> std::io::Result<usize> {
    let now = wylde_shared::anchor::epoch_now();
    for c in &mut concepts {
        if c.created_at <= 0.0 {
            c.created_at = now;
        }
        c.updated_at = now;
    }
    let n = concepts.len();
    save(workspace_id, &concepts)?;
    Ok(n)
}

/// Fields a caller may patch on an existing concept. `None` leaves the field
/// untouched; `Some` replaces it wholesale.
#[derive(Clone, Debug, Default)]
pub struct ConceptPatch {
    pub label: Option<String>,
    pub description: Option<String>,
    pub members: Option<Vec<String>>,
    pub member_files: Option<Vec<String>>,
    pub parent_concepts: Option<Vec<String>>,
    pub described_by: Option<Vec<String>>,
}

/// Outcome of an [`update`]. `Updated` boxes the [`Concept`] — the struct is
/// heavy (centroid + several member lists), so a bare variant would bloat the
/// whole enum to the large variant's size (clippy `large_enum_variant`).
#[derive(Clone, Debug, PartialEq)]
pub enum UpdateOutcome {
    Updated(Box<Concept>),
    NotFound,
}

/// Patch the concept with `id`, re-stamping `updated_at`. Editing a concept
/// (relabel, re-describe, link a vocabulary term) marks it [`ConceptSource::Manual`]
/// only when the caller passes a label/description change — otherwise the
/// provenance is preserved. (Provenance change is the caller's job via a
/// dedicated path; this keeps `update` field-orthogonal.)
pub fn update(workspace_id: &str, id: &str, patch: ConceptPatch) -> std::io::Result<UpdateOutcome> {
    let mut all = load(workspace_id);
    let Some(idx) = all.iter().position(|c| c.id == id) else {
        return Ok(UpdateOutcome::NotFound);
    };
    {
        let c = &mut all[idx];
        if let Some(v) = patch.label {
            c.label = v;
        }
        if let Some(v) = patch.description {
            c.description = v;
        }
        if let Some(v) = patch.members {
            c.members = v;
        }
        if let Some(v) = patch.member_files {
            c.member_files = v;
        }
        if let Some(v) = patch.parent_concepts {
            c.parent_concepts = v;
        }
        if let Some(v) = patch.described_by {
            c.described_by = v;
        }
        c.updated_at = wylde_shared::anchor::epoch_now();
    }
    let updated = all[idx].clone();
    save(workspace_id, &all)?;
    Ok(UpdateOutcome::Updated(Box::new(updated)))
}

/// Remove the concept with `id`. Returns `true` iff one was removed.
pub fn delete(workspace_id: &str, id: &str) -> std::io::Result<bool> {
    let all = load(workspace_id);
    let before = all.len();
    let kept: Vec<Concept> = all.into_iter().filter(|c| c.id != id).collect();
    if kept.len() == before {
        return Ok(false);
    }
    save(workspace_id, &kept)?;
    Ok(true)
}

/// One concept by id.
pub fn get(workspace_id: &str, id: &str) -> Option<Concept> {
    load(workspace_id).into_iter().find(|c| c.id == id)
}

/// Concepts whose `parent_concepts` contains `parent_id` (DAG child traversal).
pub fn list_under(workspace_id: &str, parent_id: &str) -> Vec<Concept> {
    load(workspace_id)
        .into_iter()
        .filter(|c| c.parent_concepts.iter().any(|p| p == parent_id))
        .collect()
}

/// Reverse lookup (thesis §4.2): every concept whose member set contains
/// `symbol_id`. The set is many-to-many, so this can return several.
pub fn find_by_member(workspace_id: &str, symbol_id: &str) -> Vec<Concept> {
    load(workspace_id)
        .into_iter()
        .filter(|c| c.has_member(symbol_id))
        .collect()
}

/// Reverse lookup by file: every concept that touches `file`.
pub fn find_by_file(workspace_id: &str, file: &str) -> Vec<Concept> {
    load(workspace_id)
        .into_iter()
        .filter(|c| c.touches_file(file))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::concepts::concept::ConceptSource;
    use crate::test_support::TestEnv;

    fn concept(id: &str) -> Concept {
        let mut c = Concept::new(id, id, format!("desc {id}"), ConceptSource::DirectoryCluster);
        c.members = vec![format!("{id}_sym")];
        c.member_files = vec![format!("src/{id}.rs")];
        c
    }

    #[test]
    fn create_then_load_round_trips() {
        let _env = TestEnv::new();
        let ws = "ws-con-000000";
        let out = create(ws, concept("graph")).unwrap();
        assert!(matches!(out, CreateOutcome::Created(_)));
        let back = load(ws);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].id, "graph");
    }

    #[test]
    fn create_rejects_duplicate_id() {
        let _env = TestEnv::new();
        let ws = "ws-con-dup-0000";
        create(ws, concept("dup")).unwrap();
        match create(ws, concept("dup")).unwrap() {
            CreateOutcome::AlreadyExists(c) => assert_eq!(c.id, "dup"),
            _ => panic!("expected AlreadyExists"),
        }
        assert_eq!(load(ws).len(), 1);
    }

    #[test]
    fn upsert_replaces_in_place_and_preserves_created_at() {
        let _env = TestEnv::new();
        let ws = "ws-con-ups-0000";
        let first = upsert(ws, concept("x")).unwrap();
        let mut edited = concept("x");
        edited.label = "relabeled".into();
        let second = upsert(ws, edited).unwrap();
        assert_eq!(load(ws).len(), 1, "replaced, not appended");
        assert_eq!(second.label, "relabeled");
        assert_eq!(second.created_at, first.created_at, "created_at preserved");
        assert!(second.updated_at >= first.updated_at);
    }

    #[test]
    fn replace_all_swaps_the_set() {
        let _env = TestEnv::new();
        let ws = "ws-con-repl-000";
        create(ws, concept("old")).unwrap();
        let n = replace_all(ws, vec![concept("a"), concept("b")]).unwrap();
        assert_eq!(n, 2);
        let ids: Vec<String> = load(ws).into_iter().map(|c| c.id).collect();
        assert_eq!(ids, vec!["a", "b"], "old set replaced");
    }

    #[test]
    fn update_patches_and_restamps() {
        let _env = TestEnv::new();
        let ws = "ws-con-upd-0000";
        create(ws, concept("edit")).unwrap();
        let patched = match update(
            ws,
            "edit",
            ConceptPatch {
                description: Some("new".into()),
                described_by: Some(vec!["Term".into()]),
                ..Default::default()
            },
        )
        .unwrap()
        {
            UpdateOutcome::Updated(c) => c,
            o => panic!("expected Updated, got {o:?}"),
        };
        assert_eq!(patched.description, "new");
        assert_eq!(patched.described_by, vec!["Term"]);
        assert_eq!(update(ws, "ghost", ConceptPatch::default()).unwrap(), UpdateOutcome::NotFound);
    }

    #[test]
    fn delete_removes_and_reports() {
        let _env = TestEnv::new();
        let ws = "ws-con-del-0000";
        create(ws, concept("a")).unwrap();
        create(ws, concept("b")).unwrap();
        assert!(delete(ws, "a").unwrap());
        assert_eq!(load(ws).len(), 1);
        assert!(!delete(ws, "a").unwrap());
    }

    #[test]
    fn list_under_traverses_dag() {
        let _env = TestEnv::new();
        let ws = "ws-con-hier-000";
        create(ws, concept("auth")).unwrap();
        let mut child = concept("token_auth");
        child.parent_concepts = vec!["auth".into(), "http".into()];
        create(ws, child).unwrap();
        // A child with multiple parents shows under each.
        assert_eq!(list_under(ws, "auth").len(), 1);
        assert_eq!(list_under(ws, "http").len(), 1);
        assert!(list_under(ws, "token_auth").is_empty());
    }

    #[test]
    fn reverse_lookup_by_member_and_file() {
        let _env = TestEnv::new();
        let ws = "ws-con-rev-0000";
        let mut a = concept("a");
        a.members = vec!["shared_sym".into()];
        a.member_files = vec!["src/shared.rs".into()];
        let mut b = concept("b");
        b.members = vec!["shared_sym".into()]; // overlap: many-to-many
        b.member_files = vec!["src/other.rs".into()];
        create(ws, a).unwrap();
        create(ws, b).unwrap();
        let by_member = find_by_member(ws, "shared_sym");
        assert_eq!(by_member.len(), 2, "overlap surfaces both concepts");
        let by_file = find_by_file(ws, "src/shared.rs");
        assert_eq!(by_file.len(), 1);
        assert_eq!(by_file[0].id, "a");
        assert!(find_by_member(ws, "missing").is_empty());
    }

    #[test]
    fn load_is_empty_for_missing_file() {
        let _env = TestEnv::new();
        assert!(load("nope-000000").is_empty());
    }
}
