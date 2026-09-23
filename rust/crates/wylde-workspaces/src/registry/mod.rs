//! `registry/` — the store of workspace configurations.
//!
//! **Conceptual path:** `Core/Harness/Workspaces/Registry/`.
//!
//! This is the maintainer's "workspaces memory of some sort that stores each
//! workspace configuration" — the *config* tier, distinct from the
//! per-workspace memory-entries layer in [`super::memory`].
//!
//! ## On-disk layout (Q5 clean break — see the design doc §4)
//!
//! Per-workspace config is split into its own folder; a single index
//! file carries the active pointer + MRU:
//!
//! ```text
//! <data_dir>/workspaces/
//! ├── index.json              # WorkspaceState: { active_id, mru }
//! └── <workspace_id>/
//!     ├── definition.json     # this workspace's WorkspaceDefinition
//!     ├── persona.md          # persona override (super::super::persona)
//!     └── memory.jsonl        # workspace-memory entries (super::super::memory)
//! ```
//!
//! `index.json`'s `mru` list drives the dropdown and its recency ordering,
//! and is **unbounded** — a workspace leaves the registry only by explicit
//! `delete`, never by being pushed past the window (#133). But the `mru` list
//! is only a *cache* of which workspaces exist; the authoritative record is
//! the set of bundle directories on disk. [`list_all`] walks them
//! ([`persistence::list_bundle_ids`]) so a lost, stale, or damaged index can
//! no longer orphan a bundle (#134), reconciling stale `mru` entries back out
//! of the index.
//!
//! ## Split
//!
//! * [`definition`] — the [`WorkspaceDefinition`] type + its
//!   `definition.json` IO.
//! * [`slug`] — `slug_for(folder)` deterministic id derivation (moved
//!   here from the retired `crate::memory::workspaces`).
//! * [`persistence`] — folder helpers + per-workspace `definition.json`
//!   load/save/delete + a registry-wide `load_all`.
//! * [`state`] — the [`WorkspaceState`] (active + MRU-5) + `index.json`
//!   IO + the MRU state machine.

pub mod definition;
pub mod pending;
pub mod persistence;
pub mod slug;
pub mod state;

pub use definition::WorkspaceDefinition;
pub use slug::slug_for;
pub use state::{WorkspaceState, MRU_WINDOW};

/// Unix epoch seconds, `f64` to match the timestamp convention used by
/// the rest of the harness memory layer.
///
/// Rounded to milliseconds: full `as_secs_f64()` nanosecond precision
/// sits at the f64 significand boundary for epoch-scale values, so it
/// can shift by 1 ULP across a JSON serialize→parse round-trip (which
/// would break exact-equality reload checks). Millisecond precision is
/// far finer than any workspace timestamp needs and serializes to a
/// short, unambiguous decimal that round-trips exactly.
pub fn epoch_now() -> f64 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    (secs * 1000.0).round() / 1000.0
}

/// Errors surfaced by the registry facade. The IPC layer maps these to
/// `Reply::err_msg` codes.
#[derive(Debug, thiserror::Error)]
pub enum RegistryError {
    /// No workspace with that id is registered.
    #[error("workspace not found: {0}")]
    NotFound(String),

    /// `index.json` exists but is unreadable or corrupt, so the set of
    /// registered workspaces is UNKNOWN (#140). Distinct from "no workspaces":
    /// callers must surface this rather than render an empty list, because the
    /// real list is still on disk and a write-back would destroy it.
    #[error("workspace index damaged: {0}")]
    IndexDamaged(#[from] state::StateError),
}

// ── Registry facade ────────────────────────────────────────────────────
//
// The verb handlers in [`super::api`] drive the registry through these
// functions; they compose `persistence` (per-workspace `definition.json`)
// with `state` (the `index.json` active+MRU machine) and keep the two in
// sync. A workspace's on-disk bundle is destroyed only by explicit
// [`delete`] — never as a side effect of registering another one (#133).

/// One workspace's config, or `None` if unregistered.
pub fn get(workspace_id: &str) -> Option<WorkspaceDefinition> {
    persistence::load_definition(workspace_id)
}

/// Up to [`MRU_WINDOW`] workspace definitions in MRU order plus the
/// active id — exactly what the InferenceBar dropdown renders.
///
/// `Err(IndexDamaged)` when `index.json` exists but can't be read or parsed.
/// This deliberately does NOT degrade to an empty list (#140): "your
/// workspaces are gone" and "I can't read the file that lists them" look
/// identical to a user but are opposite situations, and only one of them is
/// recoverable by leaving the disk alone.
pub fn list_mru() -> Result<(Vec<WorkspaceDefinition>, Option<String>), RegistryError> {
    let state = state::load()?;
    let defs = state
        .mru
        .iter()
        .take(MRU_WINDOW)
        .filter_map(|id| persistence::load_definition(id))
        .collect();
    Ok((defs, state.active_id))
}

/// Every workspace that exists **on disk**, MRU-ordered where the index is
/// readable and disk-ordered when it is not (#134).
///
/// Unlike [`list_mru`] — the bounded, index-only dropdown feed — this is a
/// disk-walk ([`persistence::list_bundle_ids`]): it surfaces bundles that
/// never entered, or have fallen out of, `index.json`, so a lost or stale
/// index can no longer orphan a workspace. Because everything it returns is a
/// live workspace id, each is deletable through the same [`delete`] verb.
///
/// When the index is readable it also self-heals in two ways:
///   * **reconcile stale entries** — an `mru` id (or the active pointer) whose
///     bundle is gone is dropped from the persisted index rather than
///     occupying a dropdown slot forever;
///   * **surface orphans** — a bundle on disk but missing from the index is
///     appended after the MRU-ordered ids.
///
/// When the index is *damaged* it is left untouched (that is #140's guard) and
/// the list is recovered straight from disk — logged loudly, never folded to
/// an empty "no workspaces".
pub fn list_all() -> Vec<WorkspaceDefinition> {
    let on_disk = persistence::list_bundle_ids();
    let ordered = match state::load() {
        Ok(mut state) => {
            if reconcile_against_disk(&mut state, &on_disk) {
                let _ = state::save(&state);
            }
            let mut ids = state.mru.clone();
            for id in &on_disk {
                if !state.mru.contains(id) {
                    ids.push(id.clone());
                }
            }
            ids
        }
        Err(e) => {
            tracing::error!(
                "workspaces.registry: index damaged ({e}); recovering the workspace \
                 list from the on-disk bundles rather than presenting an empty registry"
            );
            on_disk
        }
    };
    ordered
        .iter()
        .filter_map(|id| persistence::load_definition(id))
        .collect()
}

/// Drop from `state` any MRU id (and the active pointer) whose bundle no
/// longer exists on disk. Returns whether anything changed, so the caller
/// persists only a real repair. This is what stops a stale manifest entry
/// from silently costing a dropdown slot forever (#134).
fn reconcile_against_disk(state: &mut WorkspaceState, on_disk: &[String]) -> bool {
    let present: std::collections::HashSet<&String> = on_disk.iter().collect();
    let before = state.mru.len();
    state.mru.retain(|id| present.contains(id));
    let mut changed = state.mru.len() != before;
    if let Some(active) = state.active_id.as_deref() {
        if !present.contains(&active.to_owned()) {
            state.active_id = None;
            changed = true;
        }
    }
    changed
}

/// Register a folder as a workspace (idempotent on the derived id) and
/// promote it to the active/MRU head. Re-registering an existing folder
/// preserves its `created_at` and feature toggles; an explicit `name`
/// overrides the stored display name.
/// `Err(IndexDamaged)` when `index.json` can't be read (#140). The check runs
/// BEFORE `definition.json` is written: registering a workspace we then can't
/// add to the index would leave an orphaned bundle that nothing lists.
pub fn create(folder: &str, name: Option<&str>) -> Result<WorkspaceDefinition, RegistryError> {
    // Fail before writing anything if the index is unusable.
    let _ = state::load()?;
    let mut def = WorkspaceDefinition::new(folder);
    if let Some(existing) = persistence::load_definition(&def.id) {
        def.created_at = existing.created_at;
        def.persona_enabled = existing.persona_enabled;
        def.rag_enabled = existing.rag_enabled;
        def.name = existing.name;
    }
    if let Some(n) = name.map(str::trim).filter(|s| !s.is_empty()) {
        def.name = n.to_owned();
    }
    def.updated_at = epoch_now();
    let _ = persistence::save_definition(&def);
    promote_and_persist(&def.id)?;
    Ok(def)
}

/// Mark an existing workspace active (and bump MRU). `Err(NotFound)`
/// when the id is unknown.
pub fn set_active(workspace_id: &str) -> Result<WorkspaceState, RegistryError> {
    if persistence::load_definition(workspace_id).is_none() {
        return Err(RegistryError::NotFound(workspace_id.to_owned()));
    }
    promote_and_persist(workspace_id)?;
    Ok(state::load()?)
}

/// Rename / toggle features. Returns the updated definition, or `None`
/// when the id is unknown.
pub fn update(
    workspace_id: &str,
    name: Option<&str>,
    persona_enabled: Option<bool>,
    rag_enabled: Option<bool>,
) -> Option<WorkspaceDefinition> {
    let mut def = persistence::load_definition(workspace_id)?;
    if let Some(n) = name.map(str::trim).filter(|s| !s.is_empty()) {
        def.name = n.to_owned();
    }
    if let Some(p) = persona_enabled {
        def.persona_enabled = p;
    }
    if let Some(r) = rag_enabled {
        def.rag_enabled = r;
    }
    def.updated_at = epoch_now();
    let _ = persistence::save_definition(&def);
    Some(def)
}

/// Remove a workspace from the index and delete its on-disk bundle.
/// Returns `Ok(false)` if it wasn't registered.
///
/// `Err(IndexDamaged)` when the index can't be read (#140) — and note that
/// this returns BEFORE the teardown. Tearing a bundle down on the strength of
/// an index we couldn't read is precisely the destructive move: the teardown
/// is irreversible, so it must not proceed on unknown state.
pub fn delete(workspace_id: &str) -> Result<bool, RegistryError> {
    let existed = persistence::load_definition(workspace_id).is_some();
    let mut state = state::load()?;
    state.forget(workspace_id);
    if let Err(e) = state::save(&state) {
        tracing::error!("workspaces.registry: delete could not persist index: {e}");
        return Err(RegistryError::IndexDamaged(state::StateError::Unreadable {
            path: state::index_path(),
            source: e,
        }));
    }
    teardown_bundle(workspace_id);
    if existed {
        // #166 — an EXPLICIT delete additionally sweeps the peer-service stores
        // beyond the graph: the durable workspace-memory tier (#135) and the
        // bound flat-store conversations. Both ride the same durable queue as
        // the graph cascade above, enqueued here rather than in
        // `teardown_bundle`. Since #133 that distinction is belt-and-braces —
        // `delete` is now the only caller of `teardown_bundle` (registering a
        // workspace no longer evicts anything) — but keeping the peer-service
        // sweeps scoped to `delete` documents that ONLY an explicit delete may
        // touch these stores. The old handler fired them as fire-and-forget
        // `tokio::spawn`s that a down harness silently dropped; queuing them
        // makes the sweep retry until it lands (re-attaching memories the user
        // deleted is the #166 privacy bug).
        pending::enqueue(workspace_id, pending::TeardownTarget::Memory);
        pending::enqueue(workspace_id, pending::TeardownTarget::Conversations);
    }
    Ok(existed)
}

/// Promote `id` to the active/MRU head and persist `index.json`.
///
/// #133: promotion is **non-destructive**. Registering or activating a
/// workspace only re-orders the (unbounded) MRU enumeration — it never
/// evicts, and therefore never tears down, an older workspace's bundle. The
/// only path that destroys a bundle is explicit [`delete`].
///
/// `Err(IndexDamaged)` when the index can't be read or persisted (#140): the
/// promotion is abandoned rather than written over an index we couldn't read.
fn promote_and_persist(workspace_id: &str) -> Result<(), RegistryError> {
    let mut state = state::load()?;
    state.promote(workspace_id);
    if let Err(e) = state::save(&state) {
        tracing::error!("workspaces.registry: promote could not persist index: {e}");
        return Err(RegistryError::IndexDamaged(state::StateError::Unreadable {
            path: state::index_path(),
            source: e,
        }));
    }
    Ok(())
}

/// The single teardown primitive **every** removal path funnels through:
/// drop the on-disk bundle AND enqueue the workspace for a durable graph
/// cascade (Chunk + now-orphan Entity prune, [`crate::graph::cleanup`]).
///
/// Centralising it here is the structural guarantee #99 asks for: explicit
/// `delete` — and any future removal path — cascades to the graph *by
/// construction*, because it can only remove a bundle by calling this. Since
/// #133 the sole caller is `delete`: registering a workspace no longer evicts
/// or destroys anything. The graph prune itself is durable, not fire-and-forget: the id stays
/// on the [`pending`] queue until the async drain confirms the teardown
/// landed, so a transient graph outage can't permanently orphan a workspace's
/// nodes. (Cross-ref #28 — a workspace id derives from its folder and can
/// never be repointed, so teardown must cascade rather than repoint.)
fn teardown_bundle(workspace_id: &str) {
    let _ = persistence::delete_workspace_dir(workspace_id);
    pending::enqueue(workspace_id, pending::TeardownTarget::Graph);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestEnv;

    #[test]
    fn create_registers_activates_and_is_idempotent() {
        let _env = TestEnv::new();
        let folder = _env.ws_path("proj-a");
        let a = create(&folder, None).unwrap();
        assert_eq!(
            state::load().unwrap().active_id.as_deref(),
            Some(a.id.as_str())
        );

        // Re-create the same folder: same id, created_at preserved.
        let a2 = create(&folder, Some("Renamed")).unwrap();
        assert_eq!(a2.id, a.id);
        assert_eq!(a2.created_at, a.created_at);
        assert_eq!(a2.name, "Renamed");
        let (defs, _) = list_mru().unwrap();
        assert_eq!(defs.len(), 1, "idempotent create must not duplicate");
    }

    /// #133 acceptance criterion 1: registering a 6th (and beyond) workspace
    /// must NOT destroy the least-recently-used one's bundle. Before the fix
    /// this `remove_dir_all`'d w0's entire bundle — definition, persona,
    /// memory, and RAG chunks — the moment the 6th workspace was created.
    #[test]
    fn create_past_mru_window_preserves_the_lru_bundle() {
        let _env = TestEnv::new();
        // Register the first workspace and plant the full bundle a real one
        // accumulates: persona, workspace memory, and a RAG chunk store.
        let first = create(&_env.ws_path("w0"), None).unwrap();
        let dir = persistence::workspace_dir(&first.id);
        std::fs::write(dir.join("persona.md"), b"# persona\nirreplaceable").unwrap();
        std::fs::write(dir.join("memory.jsonl"), b"{\"entry\":\"hand-authored\"}\n").unwrap();
        std::fs::create_dir_all(dir.join("index")).unwrap();
        std::fs::write(dir.join("index").join("chunks.jsonl"), b"{\"chunk\":0}\n").unwrap();

        // Register enough further workspaces to push w0 well past the window.
        let mut ids = vec![first.id.clone()];
        for i in 1..MRU_WINDOW + 2 {
            ids.push(create(&_env.ws_path(&format!("w{i}")), None).unwrap().id);
        }

        // w0's entire bundle survives on disk — nothing was torn down.
        assert!(
            get(&first.id).is_some(),
            "the LRU definition must still load"
        );
        assert!(dir.join("definition.json").exists());
        assert!(dir.join("persona.md").exists(), "persona must survive");
        assert!(dir.join("memory.jsonl").exists(), "memory must survive");
        assert!(
            dir.join("index").join("chunks.jsonl").exists(),
            "the RAG chunk store must survive"
        );

        // Every bundle survives; the dropdown still renders only the window,
        // while the full enumeration retains all of them.
        for id in &ids {
            assert!(persistence::workspace_dir(id).exists());
        }
        let (defs, _) = list_mru().unwrap();
        assert_eq!(defs.len(), MRU_WINDOW, "dropdown still renders the window");
        assert_eq!(
            persistence::load_all().len(),
            ids.len(),
            "all still enumerable"
        );
    }

    #[test]
    fn set_active_errors_on_unknown_id() {
        let _env = TestEnv::new();
        assert!(matches!(
            set_active("nope-000000"),
            Err(RegistryError::NotFound(_))
        ));
    }

    #[test]
    fn update_toggles_and_renames() {
        let _env = TestEnv::new();
        let def = create(&_env.ws_path("upd"), None).unwrap();
        let updated = update(&def.id, Some("New Name"), Some(true), Some(false)).unwrap();
        assert_eq!(updated.name, "New Name");
        assert!(updated.persona_enabled);
        assert!(!updated.rag_enabled);
        // Persisted.
        assert_eq!(get(&def.id).unwrap(), updated);
    }

    #[test]
    fn delete_forgets_and_returns_existed() {
        let _env = TestEnv::new();
        let def = create(&_env.ws_path("del-me"), None).unwrap();
        assert!(delete(&def.id).unwrap());
        assert!(get(&def.id).is_none());
        assert!(state::load().unwrap().active_id.is_none());
        assert!(!delete("nope-000000").unwrap());
    }

    // ── #99: BOTH removal paths cascade to the graph via one primitive ───

    /// Targets queued for a given workspace id, as a sorted list.
    fn queued_targets(id: &str) -> Vec<pending::TeardownTarget> {
        let mut ts: Vec<_> = pending::list()
            .into_iter()
            .filter(|e| e.workspace_id == id)
            .map(|e| e.target)
            .collect();
        ts.sort();
        ts
    }

    #[test]
    fn delete_enqueues_the_graph_teardown() {
        let _env = TestEnv::new();
        let def = create(&_env.ws_path("del-cascade"), None).unwrap();
        assert!(pending::list().is_empty(), "clean slate");
        delete(&def.id).unwrap();
        assert!(
            queued_targets(&def.id).contains(&pending::TeardownTarget::Graph),
            "explicit delete must enqueue the graph cascade"
        );
    }

    /// #166 — an explicit delete must ALSO durably queue the memory + conversation
    /// sweeps. Before #166 these were fire-and-forget and nothing was queued;
    /// this asserts all three targets now are. (Since #133 the ONLY teardown
    /// path is explicit delete — registering never evicts — so there is no
    /// eviction path that would need to preserve the memory tier separately.)
    #[test]
    fn delete_enqueues_the_peer_service_sweeps() {
        let _env = TestEnv::new();
        let def = create(&_env.ws_path("del-sweeps"), None).unwrap();
        assert!(pending::list().is_empty(), "clean slate");
        delete(&def.id).unwrap();
        assert_eq!(
            queued_targets(&def.id),
            vec![
                pending::TeardownTarget::Graph,
                pending::TeardownTarget::Memory,
                pending::TeardownTarget::Conversations,
            ],
            "explicit delete must durably queue graph + memory + conversation sweeps"
        );
    }

    /// #133: the flip side of the graph cascade. Because registering past the
    /// window no longer evicts anything, it must **not** enqueue a graph
    /// teardown either — the old code cascaded a destructive eviction here.
    /// This is also the #166 guarantee that a non-delete path queues NO sweep
    /// of any target (graph, memory, or conversations).
    #[test]
    fn create_past_mru_window_enqueues_no_teardown() {
        let _env = TestEnv::new();
        let first = create(&_env.ws_path("w0"), None).unwrap();
        for i in 1..MRU_WINDOW + 2 {
            create(&_env.ws_path(&format!("w{i}")), None).unwrap();
        }
        assert!(get(&first.id).is_some(), "w0 survives past the window");
        assert!(
            pending::list().is_empty(),
            "non-destructive registration must not enqueue any teardown"
        );
    }

    // ── #134: enumeration walks disk, not just the index manifest ──────────

    /// Criterion 1 (plant-and-find): a complete bundle on disk with **no**
    /// index entry must still be surfaced by the enumeration. Fails before —
    /// every listing path read only `index.json`'s `mru`.
    #[test]
    fn list_all_surfaces_a_bundle_absent_from_the_index() {
        let _env = TestEnv::new();
        let registered = create(&_env.ws_path("registered"), None).unwrap();
        // Plant an orphan straight to disk — a real bundle the index never saw.
        let orphan = WorkspaceDefinition::new(&_env.ws_path("orphan-on-disk"));
        persistence::save_definition(&orphan).unwrap();
        assert!(
            !state::load().unwrap().mru.contains(&orphan.id),
            "the orphan is deliberately absent from the index"
        );

        let ids: Vec<String> = list_all().into_iter().map(|d| d.id).collect();
        assert!(ids.contains(&registered.id));
        assert!(
            ids.contains(&orphan.id),
            "the disk-walk must surface a bundle the index never knew about"
        );
    }

    /// Criterion 2 (corrupt-index recovery): a damaged index must not fold the
    /// enumeration to empty — it recovers the full set from the bundles on disk.
    #[test]
    fn list_all_recovers_from_a_corrupt_index_instead_of_reading_empty() {
        let _env = TestEnv::new();
        let a = create(&_env.ws_path("a"), None).unwrap();
        let b = create(&_env.ws_path("b"), None).unwrap();
        // Corrupt the index the way a torn write would.
        std::fs::write(state::index_path(), b"{ not valid json").unwrap();
        assert!(state::load().is_err(), "the index is now damaged");

        let ids: Vec<String> = list_all().into_iter().map(|d| d.id).collect();
        assert_eq!(ids.len(), 2, "must recover every bundle, not read empty");
        assert!(ids.contains(&a.id) && ids.contains(&b.id));
    }

    /// Criterion 4 (reconcile): a stale `mru` id whose bundle is gone must be
    /// dropped from the *persisted* index rather than occupying a slot forever.
    #[test]
    fn list_all_reconciles_a_stale_index_entry_out_of_the_persisted_mru() {
        let _env = TestEnv::new();
        let real = create(&_env.ws_path("real"), None).unwrap();
        // Inject a ghost id (no bundle dir) into the persisted index.
        let mut st = state::load().unwrap();
        st.mru.insert(0, "ghost-000000".to_owned());
        state::save(&st).unwrap();

        let _ = list_all();

        let after = state::load().unwrap();
        assert!(
            !after.mru.contains(&"ghost-000000".to_owned()),
            "a stale id with no bundle must be dropped from the persisted index"
        );
        assert!(after.mru.contains(&real.id), "the real workspace stays");
    }

    /// Criterion 5 (found-is-deletable): a workspace discovered only by the
    /// disk-walk is removable through the same `delete` verb — discovery and
    /// the delete affordance stay paired (the #131 bar).
    #[test]
    fn a_disk_walked_orphan_is_deletable_through_the_delete_verb() {
        let _env = TestEnv::new();
        let orphan = WorkspaceDefinition::new(&_env.ws_path("orphan-del"));
        persistence::save_definition(&orphan).unwrap();
        assert!(
            list_all().iter().any(|d| d.id == orphan.id),
            "the orphan is discovered by the disk-walk"
        );

        assert!(delete(&orphan.id).unwrap(), "the discovered orphan existed");
        assert!(
            !persistence::workspace_dir(&orphan.id).exists(),
            "bundle removed"
        );
        assert!(
            !list_all().iter().any(|d| d.id == orphan.id),
            "and it is gone from the enumeration after delete"
        );
    }
}
