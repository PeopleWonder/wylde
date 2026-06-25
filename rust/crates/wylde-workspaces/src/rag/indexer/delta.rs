//! Per-file delta re-index — the file-watcher's hot path (Slice I).
//!
//! Where [`super::reindex_full`] / [`super::reindex_delta`] re-walk the whole
//! workspace folder, this re-indexes **one changed file** so a single save
//! lands in the graph well inside the plan's <500ms budget (no folder walk, a
//! single `treesitter.extract_entities` hop, two small Bolt round-trips).
//!
//! Two entry points, dispatched by the watcher off the debounced event kind:
//!   * [`upsert_file`] — a created/modified file: clear its stale graph nodes,
//!     re-extract + upsert this one file, refresh its vector chunks.
//!   * [`remove_file`] — a deleted/renamed-away file (or a deleted directory's
//!     subtree): drop its graph footprint (pruning now-orphaned entities) and
//!     its vector chunks.
//!
//! Both reuse the same machinery the full passes use ([`graph_writer`],
//! [`super::embed_chunks`], [`store`]) so the on-disk + on-graph shapes are
//! byte-identical to a full reindex — a delta and a full pass over the same
//! tree converge. Graph-write and vector-embed are independent and each
//! fail-soft: a tree-sitter/Neo4j outage still lets the vector half run, and
//! an Ollama outage still lets the graph half land (the graph is the watcher's
//! primary deliverable — it needs no embedder).

use crate::graph::BoltClient;
use crate::rag::LexicalConfig;
use crate::registry::{self, WorkspaceDefinition};

use super::store::{self, IndexedChunk};
use super::{embed_chunks, graph_writer, lexical, lock, manifest, walk};

/// What one delta did, for the watcher's log line + the completion event.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DeltaOutcome {
    /// `"upsert"`, `"remove"`, or `"skip"`.
    pub action: &'static str,
    /// The canonical path acted on (matches the chunk-store key).
    pub path: String,
    /// Chunk nodes the graph-write upserted (0 for a remove / prose file).
    pub graph_chunk_nodes: u32,
    /// Vector chunks (re)embedded into the file index (0 for a remove).
    pub chunks_indexed: u32,
    /// First non-fatal graph-write failure (sidecar/backend outage), if any.
    pub graph_error: Option<String>,
    /// Non-fatal vector-update failure (embedder unreachable), if any.
    pub vector_error: Option<String>,
    /// Set when the file was filtered out before any work (with the reason).
    pub skipped_reason: Option<&'static str>,
}

impl DeltaOutcome {
    fn skipped(path: &str, reason: &'static str) -> Self {
        Self {
            action: "skip",
            path: path.to_owned(),
            skipped_reason: Some(reason),
            ..Default::default()
        }
    }
}

/// Re-index a single created/modified file. Filtered files (skip-dir / hidden
/// / binary-suffix, then the content sniff) are skipped; a file that no longer
/// yields chunks (became empty/binary) is treated as a removal so its stale
/// index is cleared rather than left behind.
pub async fn upsert_file(def: &WorkspaceDefinition, path: &str) -> DeltaOutcome {
    if !walk::is_indexable_path(&def.folder, path) {
        tracing::debug!("workspaces.watcher: skip filtered path {path}");
        return DeltaOutcome::skipped(path, "filtered");
    }
    // Content-hash short-circuit (P3): a save that didn't change the bytes
    // (touch / editor re-save / checkout) hashes identically to the manifest
    // entry — skip the embed round-trip + graph rewrite, just refresh the
    // recorded mtime so the fast-path stays warm.
    if let Some((hash, size, mtime)) = walk::hash_file(path) {
        let canonical = walk::canonical_path(std::path::Path::new(path));
        if manifest::file_hash(&def.id, &canonical).as_deref() == Some(hash.as_str()) {
            if registry::get(&def.id).is_some() {
                let lk = lock::for_workspace(&def.id);
                let _g = lk.lock().await;
                manifest::touch_file(&def.id, &canonical, size, mtime);
            }
            tracing::debug!("workspaces.watcher: {canonical} unchanged (hash match); skip re-embed");
            return DeltaOutcome::skipped(&canonical, "unchanged");
        }
    }
    let chunks = walk::chunk_one_file(path);
    if chunks.is_empty() {
        // Exists but unparseable as text now (empty / binary / unreadable):
        // clear any prior index for it instead of leaving stale rows.
        tracing::debug!("workspaces.watcher: {path} yielded no chunks; clearing prior index");
        return remove_file(def, path).await;
    }
    // Every chunk shares the file's canonical path (chunk_one_file canonicalises).
    let canonical = chunks[0].path.clone();

    let mut outcome = DeltaOutcome {
        action: "upsert",
        path: canonical.clone(),
        ..Default::default()
    };

    // ── Graph half (Ollama-independent — runs first, like the full passes) ──
    // Clear stale Chunk nodes for this path BEFORE re-ingesting: a changed
    // mtime changes the chunk id, so a bare MERGE would orphan the old nodes.
    // prune_orphans=false — skip the global Entity sweep on a modify; the
    // entities are about to be re-MERGE'd by write_graph, and skipping the
    // full-graph scan keeps the per-file delta cheap.
    let bolt = BoltClient::new();
    let cleared = bolt.delete_file_nodes(&def.id, &canonical, false).await;
    if !cleared.ok {
        if let Some(e) = &cleared.error {
            outcome.graph_error = Some(format!("clear stale nodes: {} {}", e.code, e.message));
        }
    }
    let g = graph_writer::write_graph(def, &chunks).await;
    outcome.graph_chunk_nodes = g.chunk_nodes;
    if outcome.graph_error.is_none() {
        outcome.graph_error = g.error;
    }

    // ── Vector half (best-effort — needs Ollama) ───────────────────────────
    match vector_upsert(&def.id, &canonical, &chunks).await {
        Ok(n) => outcome.chunks_indexed = n,
        Err(e) => outcome.vector_error = Some(e),
    }

    outcome
}

/// Drop a deleted/renamed-away file (or a deleted directory's whole subtree)
/// from both stores. Idempotent — removing a path that was never indexed is a
/// no-op (0 deleted).
pub async fn remove_file(def: &WorkspaceDefinition, path: &str) -> DeltaOutcome {
    // A removed path can't be canonicalised by `canonicalize()` (it's gone),
    // so use the lenient (parent + name) form — the same string the walk
    // stored while the file existed.
    let canonical = walk::canonical_path(std::path::Path::new(path));
    let mut outcome = DeltaOutcome {
        action: "remove",
        path: canonical.clone(),
        ..Default::default()
    };

    // Graph: drop the file's (or subtree's) chunks AND prune now-orphaned
    // entities — a symbol only this file mentioned should disappear.
    let del = BoltClient::new()
        .delete_file_nodes(&def.id, &canonical, true)
        .await;
    if !del.ok {
        if let Some(e) = &del.error {
            outcome.graph_error = Some(format!("{} {}", e.code, e.message));
        }
    }

    // Vector: drop the file's chunks (and any under it, for a directory).
    if let Err(e) = vector_remove(&def.id, &canonical).await {
        outcome.vector_error = Some(e);
    }

    outcome
}

/// Replace `canonical`'s chunks in the vector index with freshly-embedded
/// ones, leaving every other file untouched. Returns the count re-embedded.
/// Existence-guarded so a delete that races the watcher doesn't recreate a
/// gone workspace's bundle.
async fn vector_upsert(
    workspace_id: &str,
    canonical: &str,
    chunks: &[walk::Chunk],
) -> Result<u32, String> {
    // No live-progress reporter for the watcher's per-file delta — it's a tiny,
    // fast upsert, not a user-initiated full reindex with a bar.
    let fresh = embed_chunks(chunks.to_vec(), None).await?;
    if registry::get(workspace_id).is_none() {
        return Ok(0);
    }
    let ids: Vec<String> = fresh.iter().map(|c| c.id.clone()).collect();
    // Capture this file's fresh chunks for the lexical incremental upsert before
    // they're moved into the merged set (gated, so OFF pays nothing — not even
    // the clone). A single file's chunks are a handful, so the clone is cheap.
    let lexical_on = LexicalConfig::current().enabled;
    let lexical_chunks = if lexical_on { fresh.clone() } else { Vec::new() };
    // Hold the per-workspace index lock across the chunks-then-manifest pair so
    // a racing manual reindex can't tear it (§3.3).
    let lk = lock::for_workspace(workspace_id);
    let _g = lk.lock().await;
    let mut kept: Vec<IndexedChunk> = store::load_chunks(workspace_id);
    kept.retain(|c| c.path != canonical);
    let n = fresh.len() as u32;
    kept.extend(fresh);
    store::save_chunks(workspace_id, &kept).map_err(|e| e.to_string())?;
    // Manifest second: record this file's content hash so the next watcher
    // event on it can short-circuit a no-op save.
    if let Some((hash, size, mtime)) = walk::hash_file(canonical) {
        manifest::update_file(workspace_id, canonical, hash, size, mtime, ids);
    }
    // Lexical third (best-effort, AFTER the vector write so the BM25 index never
    // holds a chunk the store lacks, §2.6): exact-path delete + add this file's
    // fresh docs. Incremental — no full rebuild on a single-file save.
    if lexical_on {
        if let Err(e) = lexical::sync_upsert_file(workspace_id, canonical, &lexical_chunks) {
            tracing::warn!("workspaces.rag.lexical: upsert {canonical} failed: {e}");
        }
    }
    Ok(n)
}

/// Remove `canonical`'s chunks (exact path or any under `<canonical><sep>`,
/// for a deleted directory) from the vector index AND its manifest entries.
/// No-op when nothing matches. Under the per-workspace index lock (§3.3).
async fn vector_remove(workspace_id: &str, canonical: &str) -> Result<(), String> {
    if registry::get(workspace_id).is_none() {
        return Ok(());
    }
    let lk = lock::for_workspace(workspace_id);
    let _g = lk.lock().await;
    // Capture the subtree's chunk ids from the manifest BEFORE it's mutated — the
    // lexical remove needs them to drop a deleted directory's docs (an exact path
    // term only reaches the single file). Gated so OFF pays nothing.
    let lexical_on = LexicalConfig::current().enabled;
    let subtree_ids = if lexical_on {
        manifest::chunk_ids_under(workspace_id, canonical)
    } else {
        Vec::new()
    };
    let mut kept = store::load_chunks(workspace_id);
    let before = kept.len();
    let prefix = format!("{canonical}{}", std::path::MAIN_SEPARATOR);
    kept.retain(|c| c.path != canonical && !c.path.starts_with(&prefix));
    if kept.len() != before {
        store::save_chunks(workspace_id, &kept).map_err(|e| e.to_string())?;
    }
    // Lexical (best-effort): drop the file by exact path + the subtree's ids.
    if lexical_on {
        if let Err(e) = lexical::sync_remove_file(workspace_id, canonical, &subtree_ids) {
            tracing::warn!("workspaces.rag.lexical: remove {canonical} failed: {e}");
        }
    }
    // Drop the manifest entry/entries either way (a stale entry with no chunks
    // is still cleaned).
    manifest::remove_files(workspace_id, canonical);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestEnv;
    use std::path::Path;

    fn idx_chunk(path: &str) -> IndexedChunk {
        IndexedChunk {
            id: format!("id-{path}"),
            path: path.to_owned(),
            chunk_idx: 0,
            content: "x".to_owned(),
            mtime: 1.0,
            start_line: 1,
            end_line: 1,
            vector: vec![0.1],
        }
    }

    /// Register a real workspace (so the existence-guard in `vector_remove`
    /// passes) and return its id. The folder is a throwaway tempdir.
    fn registered_ws(env: &TestEnv, name: &str) -> String {
        registry::create(&env.ws_path(name), None).id
    }

    #[tokio::test]
    async fn vector_remove_drops_exact_path_and_subtree_only() {
        let env = TestEnv::new();
        let ws = registered_ws(&env, "rm");
        // Canonicalise a real dir so the separators match what retain compares.
        let td = tempfile::tempdir().unwrap();
        let base = walk::canonical_path(td.path());
        let f = format!("{base}{}a.rs", std::path::MAIN_SEPARATOR);
        let sub = format!("{base}{sep}sub{sep}b.rs", sep = std::path::MAIN_SEPARATOR);
        let other = format!("{base}{}c.rs", std::path::MAIN_SEPARATOR);
        store::save_chunks(&ws, &[idx_chunk(&f), idx_chunk(&sub), idx_chunk(&other)]).unwrap();

        // Removing the `sub` directory drops everything under it (the b.rs
        // chunk), leaving the siblings a.rs + c.rs untouched.
        let dir = format!("{base}{}sub", std::path::MAIN_SEPARATOR);
        vector_remove(&ws, &dir).await.unwrap();
        let left: Vec<String> = store::load_chunks(&ws)
            .into_iter()
            .map(|c| c.path)
            .collect();
        assert!(left.contains(&f), "sibling file survives");
        assert!(left.contains(&other), "sibling file survives");
        assert!(!left.iter().any(|p| p == &sub), "subtree file removed");
    }

    #[tokio::test]
    async fn vector_remove_exact_file_keeps_siblings() {
        let env = TestEnv::new();
        let ws = registered_ws(&env, "rm2");
        let a = "/proj/a.rs".to_owned();
        let b = "/proj/b.rs".to_owned();
        store::save_chunks(&ws, &[idx_chunk(&a), idx_chunk(&b)]).unwrap();
        vector_remove(&ws, &a).await.unwrap();
        let left: Vec<String> = store::load_chunks(&ws)
            .into_iter()
            .map(|c| c.path)
            .collect();
        assert_eq!(left, vec![b]);
    }

    #[tokio::test]
    async fn vector_remove_unknown_workspace_is_noop() {
        let _env = TestEnv::new();
        // No registry entry → guard short-circuits, no panic, no file created.
        assert!(vector_remove("ghost-000000", "/x/y.rs").await.is_ok());
        assert!(store::load_chunks("ghost-000000").is_empty());
    }

    #[tokio::test]
    async fn vector_remove_drops_the_lexical_doc_when_enabled() {
        let env = TestEnv::new();
        let ws = registered_ws(&env, "lx-rm");
        // Enable the lexical toggle for the body of this test, reset on drop.
        LexicalConfig::persist(LexicalConfig {
            enabled: true,
            ..LexicalConfig::default()
        })
        .unwrap();

        // Seed: chunks on disk + a lexical index + a manifest carrying the
        // chunk id (so chunk_ids_under can find the subtree's delete keys).
        let a = "/proj/a.rs".to_owned();
        let b = "/proj/b.rs".to_owned();
        let mut ca = idx_chunk(&a);
        ca.content = "alpha_unique_marker".into();
        let cb = idx_chunk(&b);
        store::save_chunks(&ws, &[ca.clone(), cb.clone()]).unwrap();
        lexical::build_from_chunks(&ws, &[ca.clone(), cb.clone()]).unwrap();
        manifest::update_file(&ws, &a, "h".into(), 1, 1.0, vec![ca.id.clone()]);
        assert_eq!(lexical::search(&ws, "alpha_unique_marker", 5).len(), 1);

        // Remove a.rs: the watcher path drops it from the vector store AND the
        // lexical index (gated).
        vector_remove(&ws, &a).await.unwrap();
        assert!(
            lexical::search(&ws, "alpha_unique_marker", 5).is_empty(),
            "lexical doc dropped alongside the vector chunk"
        );

        LexicalConfig::persist(LexicalConfig::default()).unwrap();
    }

    #[tokio::test]
    async fn upsert_file_skips_filtered_paths_without_touching_stores() {
        let _env = TestEnv::new();
        let def = WorkspaceDefinition::new("/proj");
        // A path under target/ is filtered before any IO.
        let out = upsert_file(&def, "/proj/target/debug/x.rs").await;
        assert_eq!(out.action, "skip");
        assert_eq!(out.skipped_reason, Some("filtered"));
        assert_eq!(out.graph_chunk_nodes, 0);
    }

    #[test]
    fn remove_file_canonical_matches_walk_for_gone_file() {
        // The path a remove acts on equals the canonical form the walk would
        // have stored — so the graph/vector delete hits the right key even
        // though the file is already gone.
        let td = tempfile::tempdir().unwrap();
        let f = td.path().join("gone.rs");
        std::fs::write(&f, "fn x() {}").unwrap();
        let while_present = walk::canonical_path(&f);
        std::fs::remove_file(&f).unwrap();
        let acted = walk::canonical_path(Path::new(&f.to_string_lossy().into_owned()));
        assert_eq!(while_present, acted);
    }
}
