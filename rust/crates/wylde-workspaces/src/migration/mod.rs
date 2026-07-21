//! `migration/` — idempotent data migration into the new service layout
//! (Slice 0-migrate, OI-2).
//!
//! **Conceptual path:** `Core/Workspaces/Migration/`.
//!
//! Slice 0c relocates **workspace-scoped conversation storage**. Before the
//! split, every conversation — standalone *and* workspace-bound — lived flat
//! in the harness store at `<data_dir>/conversations/<id>.json`, with the
//! binding carried only by a `workspace_id` field. The new layout files
//! workspace conversations per-workspace at
//! `<data_dir>/workspaces/<workspace_id>/conversations/<id>.json` (see
//! [`crate::conversations`]); standalone ones stay flat and harness-owned.
//!
//! This module bridges the two on first startup of the new service: it scans
//! the flat dir, **moves** each conversation carrying a non-empty
//! `workspace_id` into its workspace bundle (byte-identical document), leaves
//! the standalone ones untouched, and writes a `migration_completed_v1`
//! marker so the pass is **safe to re-run** (the marker short-circuits it).
//!
//! ## Order
//!
//! Per the Build Order the per-data-type order is registry → notes →
//! conversations → anchors → indexes. Registry already moved (0b) with no
//! path shift; the **notes** tier also keeps its exact on-disk path
//! (`<data_dir>/workspaces/<id>/memory.jsonl`), so neither needs relocating —
//! only **conversations** shift directory. Anchors/indexes don't exist yet.
//! So v1 migrates conversations only.
//!
//! ## Where it runs (and where it must NOT)
//!
//! Migration runs **only in the service process** (`main.rs`, before serving).
//! It is deliberately NOT invoked from the harness in-process compat-shim
//! fallback: until the service genuinely goes live (Slice A) the harness flat
//! `conversations.*` store must keep serving every conversation unchanged.
//! Moving files out from under it before the consumers are repointed (0d)
//! would break the GUI. Gating migration to the live service keeps production
//! untouched until go-live.

use std::path::PathBuf;

use serde_json::Value;

use crate::common::{data_dir, ensure_dir};
use crate::conversations::store as conv_store;
use crate::registry::persistence::workspaces_dir;

/// Marker filename written under `<data_dir>/workspaces/` once the v1
/// migration has completed. Its presence short-circuits future runs.
pub const MARKER_V1: &str = "migration_completed_v1";

/// Marker for the one-shot index-hygiene purge (P2). Its presence
/// short-circuits the auto-purge so it self-heals each install exactly once.
pub const MARKER_INDEX_HYGIENE_V1: &str = "index_hygiene_purge_v1";

/// `<data_dir>/conversations/` — the harness flat conversation store (the
/// pre-split location). Standalone conversations stay here; workspace ones
/// are moved out by this migration.
fn flat_conversations_dir() -> PathBuf {
    data_dir().join("conversations")
}

/// `<data_dir>/workspaces/migration_completed_v1`.
fn marker_path() -> PathBuf {
    workspaces_dir().join(MARKER_V1)
}

/// Outcome of a migration pass (logged + returned for tests).
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct MigrationReport {
    /// True when the marker was already present and the pass was skipped.
    pub skipped: bool,
    /// Flat conversation files scanned.
    pub scanned: usize,
    /// Workspace-bound conversations relocated into bundle dirs.
    pub moved: usize,
    /// Standalone conversations left in place.
    pub kept_standalone: usize,
    /// Files that failed to relocate (left in place; counted, not fatal).
    pub errors: usize,
}

/// Run the v1 migration if it hasn't run yet. Idempotent: a present marker
/// makes this a no-op. Best-effort and fail-soft — a single bad file is
/// counted as an error and skipped, never aborting the pass.
pub fn run_pending() -> MigrationReport {
    if marker_path().exists() {
        return MigrationReport {
            skipped: true,
            ..Default::default()
        };
    }
    let report = migrate_conversations();
    write_marker();
    tracing::info!(
        "wylde-workspaces migration v1: scanned={} moved={} kept_standalone={} errors={}",
        report.scanned,
        report.moved,
        report.kept_standalone,
        report.errors,
    );
    report
}

/// Move every workspace-tagged conversation out of the flat store into its
/// per-workspace bundle. Standalone (no / empty `workspace_id`) files stay.
fn migrate_conversations() -> MigrationReport {
    let mut report = MigrationReport::default();
    let dir = flat_conversations_dir();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        // No flat store yet (fresh install) → nothing to migrate.
        return report;
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        if !path.is_file() {
            continue;
        }
        let Ok(raw) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(Value::Object(map)) = serde_json::from_str::<Value>(&raw) else {
            continue;
        };
        // Must have a usable id (otherwise it's the active-pointer or a stray
        // object — leave it; it isn't a conversation document).
        let has_id = map
            .get("id")
            .and_then(Value::as_str)
            .map(|s| !s.is_empty())
            .unwrap_or(false);
        if !has_id {
            continue;
        }
        report.scanned += 1;

        let ws = map
            .get("workspace_id")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let Some(ws) = ws else {
            // Standalone — stays in the harness flat store.
            report.kept_standalone += 1;
            continue;
        };

        // Relocate: write into the workspace bundle, then remove the flat
        // file. If the write fails, leave the original in place (count it).
        match conv_store::save_conversation(ws, &map) {
            Ok(()) => {
                if std::fs::remove_file(&path).is_ok() {
                    report.moved += 1;
                } else {
                    // Copied but couldn't unlink the original — the next
                    // list would show it in both places. Re-running is safe
                    // (save overwrites), so count it as an error to surface,
                    // not silently.
                    report.errors += 1;
                }
            }
            Err(e) => {
                tracing::warn!(
                    "wylde-workspaces migration v1: relocate {:?} → workspace {ws:?} failed: {e}",
                    path.file_name().unwrap_or_default(),
                );
                report.errors += 1;
            }
        }
    }
    report
}

/// Write the completion marker (best-effort — a failed marker write just
/// means the next startup re-scans, which is idempotent).
fn write_marker() {
    let dir = workspaces_dir();
    let _ = ensure_dir(&dir);
    let _ = std::fs::write(marker_path(), b"v1\n");  // best-effort marker; migration re-runs if unwritten (wylde-check: discard-result-ok)
}

/// `<data_dir>/workspaces/index_hygiene_purge_v1`.
fn hygiene_marker_path() -> PathBuf {
    workspaces_dir().join(MARKER_INDEX_HYGIENE_V1)
}

/// Outcome of the one-shot index-hygiene purge pass.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct HygieneReport {
    /// True when the marker was present and the pass was skipped.
    pub skipped: bool,
    /// Workspace bundles scanned.
    pub workspaces: usize,
    /// Total artifact chunks dropped across all workspaces.
    pub dropped: usize,
    /// Total chunks kept.
    pub kept: usize,
}

/// One-shot self-heal (index hygiene P2): the walk-time exclusion fix makes
/// **new** indexes clean, but an index built before it keeps its ~58 %
/// build-artifact chunks until something rewrites it. On first boot of a binary
/// carrying the fix, filter-purge every existing workspace index once (no
/// re-embed), then write [`MARKER_INDEX_HYGIENE_V1`] so it never re-runs.
///
/// Async because the purge best-effort graph-cleans over Bolt. Marker-gated +
/// idempotent: a present marker (or an already-clean index) makes it a no-op.
/// Per-workspace failures are isolated — they never abort the pass.
pub async fn run_index_hygiene_pending() -> HygieneReport {
    if hygiene_marker_path().exists() {
        return HygieneReport {
            skipped: true,
            ..Default::default()
        };
    }
    let mut report = HygieneReport::default();
    for id in registered_workspace_ids() {
        let Some(def) = crate::registry::get(&id) else {
            continue;
        };
        // Only touch workspaces that actually have a persisted index.
        if !crate::rag::indexer::store::has_index(&def.id) {
            continue;
        }
        report.workspaces += 1;
        let outcome = crate::rag::indexer::purge::purge_excluded(&def).await;
        report.dropped += outcome.dropped as usize;
        report.kept += outcome.kept as usize;
    }
    write_hygiene_marker();
    tracing::info!(
        "wylde-workspaces index-hygiene purge v1: workspaces={} dropped={} kept={}",
        report.workspaces,
        report.dropped,
        report.kept,
    );
    report
}

/// Every workspace id with a bundle dir under `<data_dir>/workspaces/` — the
/// id is the directory name (a `definition.json` lives inside). Used by the
/// hygiene pass to reach *all* indexes, not just the MRU-5 window.
fn registered_workspace_ids() -> Vec<String> {
    let mut ids = Vec::new();
    let Ok(entries) = std::fs::read_dir(workspaces_dir()) else {
        return ids;
    };
    for entry in entries.flatten() {
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            if let Some(name) = entry.file_name().to_str() {
                ids.push(name.to_owned());
            }
        }
    }
    ids
}

/// Write the hygiene completion marker (best-effort).
fn write_hygiene_marker() {
    let dir = workspaces_dir();
    let _ = ensure_dir(&dir);
    let _ = std::fs::write(hygiene_marker_path(), b"v1\n");  // best-effort marker; hygiene pass re-runs if unwritten (wylde-check: discard-result-ok)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestEnv;
    use serde_json::json;

    fn seed_flat(cid: &str, doc: Value) {
        let dir = flat_conversations_dir();
        ensure_dir(&dir).unwrap();
        std::fs::write(
            dir.join(format!("{cid}.json")),
            serde_json::to_string_pretty(&doc).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn moves_workspace_conversations_keeps_standalone() {
        let _env = TestEnv::new();
        seed_flat(
            "ws-conv",
            json!({"id": "ws-conv", "workspace_id": "proj-a", "messages": [], "title": "WS"}),
        );
        seed_flat(
            "solo",
            json!({"id": "solo", "messages": [], "title": "Solo"}),
        );
        seed_flat(
            "blank-ws",
            json!({"id": "blank-ws", "workspace_id": "  ", "messages": []}),
        );

        let report = run_pending();
        assert!(!report.skipped);
        assert_eq!(report.scanned, 3);
        assert_eq!(report.moved, 1, "only the workspace-tagged one moves");
        assert_eq!(report.kept_standalone, 2, "solo + blank-ws stay");
        assert_eq!(report.errors, 0);

        // Relocated into the workspace bundle, gone from flat.
        let doc = conv_store::read_conversation("proj-a", "ws-conv").expect("relocated");
        assert_eq!(doc["title"], "WS");
        assert!(!flat_conversations_dir().join("ws-conv.json").exists());
        // Standalone stays in the flat store.
        assert!(flat_conversations_dir().join("solo.json").exists());
        assert!(flat_conversations_dir().join("blank-ws.json").exists());
    }

    #[test]
    fn is_idempotent_via_marker() {
        let _env = TestEnv::new();
        seed_flat(
            "a",
            json!({"id": "a", "workspace_id": "w1", "messages": []}),
        );
        let first = run_pending();
        assert_eq!(first.moved, 1);
        assert!(marker_path().exists());

        // A new workspace conversation lands flat AFTER the marker — the
        // second pass must NOT touch it (marker short-circuits).
        seed_flat(
            "b",
            json!({"id": "b", "workspace_id": "w1", "messages": []}),
        );
        let second = run_pending();
        assert!(second.skipped, "second pass skipped via marker");
        assert_eq!(second.moved, 0);
        assert!(
            flat_conversations_dir().join("b.json").exists(),
            "untouched after marker"
        );
    }

    #[test]
    fn no_flat_store_is_a_clean_noop() {
        let _env = TestEnv::new();
        let report = run_pending();
        assert!(!report.skipped);
        assert_eq!(report.scanned, 0);
        assert_eq!(report.moved, 0);
        assert!(
            marker_path().exists(),
            "marker written even on empty install"
        );
    }

    #[tokio::test]
    async fn index_hygiene_purges_artifacts_once_then_marker_skips() {
        use crate::rag::indexer::store::{self, IndexedChunk};
        let env = TestEnv::new();
        let folder = env.ws_path("hygiene-proj");
        std::fs::create_dir_all(&folder).unwrap();
        let def = crate::registry::create(&folder, None).unwrap();
        let sep = std::path::MAIN_SEPARATOR;
        let mk = |rel: &str, n: u32| IndexedChunk {
            id: format!("id-{rel}-{n}"),
            path: format!("{folder}{sep}{rel}").replace('/', &sep.to_string()),
            chunk_idx: n,
            content: "x".to_owned(),
            mtime: 1.0,
            start_line: 1,
            end_line: 1,
            vector: vec![0.1],
        };
        store::save_chunks(
            &def.id,
            &[
                mk("src/main.rs", 0),
                mk("target-dev/doc/a.html", 0),
                mk("target-dev/doc/b.html", 0),
            ],
        )
        .unwrap();

        let r = run_index_hygiene_pending().await;
        assert!(!r.skipped);
        assert_eq!(r.workspaces, 1);
        assert_eq!(r.dropped, 2, "the two target-dev html chunks");
        assert_eq!(r.kept, 1, "src/main.rs survives");
        assert!(hygiene_marker_path().exists());
        assert_eq!(store::load_chunks(&def.id).len(), 1);

        // Second run is marker-skipped even though a fresh artifact chunk lands.
        store::save_chunks(
            &def.id,
            &[mk("src/main.rs", 0), mk("target-dev/doc/c.html", 0)],
        )
        .unwrap();
        let r2 = run_index_hygiene_pending().await;
        assert!(r2.skipped);
        assert_eq!(
            store::load_chunks(&def.id).len(),
            2,
            "untouched after marker"
        );
    }

    #[test]
    fn ignores_active_pointer_and_idless_objects() {
        let _env = TestEnv::new();
        // An object with no `id` (e.g. a stray pointer copy) is not a
        // conversation — never scanned/moved.
        seed_flat("pointer", json!({"some": "thing"}));
        let report = run_pending();
        assert_eq!(report.scanned, 0);
        assert_eq!(report.moved, 0);
    }
}
