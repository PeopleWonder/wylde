//! Workspace file-RAG indexer — walk → chunk → embed → store, plus k-NN
//! search and delta-reindex. Each index pass *also* drives a graph-ingest
//! pass ([`graph_writer`]) — extract structural entities + write
//! Chunk/Entity nodes + typed edges — so a workspace owns its full ingest
//! pipeline (vector + graph) end-to-end, with no N8N hop.
//!
//! Rust port of the retired Python `Core/harness/memory/workspaces/`
//! `_index.py` + `_search.py` (LanceDB), restoring the snippet-returning
//! behaviour the workspaces redesign reduced to a pointer-only stub
//! (PR #12, `bc243f2`). See `store.rs` for the storage-backend choice. The
//! graph half folds in the entity-extraction + Memgraph-write steps the
//! retired N8N `rag-ingest.json` workflow used to own (see [`graph_writer`]).
//!
//! ## Entry points
//!
//! * [`reindex`] — full if no index exists, else delta. Used by the
//!   background index-on-create/activate path and the `workspaces.reindex`
//!   verb.
//! * [`reindex_full`] / [`reindex_delta`] — the two passes.
//! * [`spawn_background_index`] — fire-and-forget index on a tokio task,
//!   guarded against the workspace being deleted mid-run.
//! * [`search::query`] — embed a query + cosine-rank the chunks.
//! * [`status`] — the [`store::RagState`] snapshot the GUI polls.
//!
//! ## Single embedding backend
//!
//! Every embedding (chunks at index time, the query at search time) goes
//! through `crate::embeddings`, i.e. the `ollama.embed` pipe verb
//! / `nomic-embed-text`. One backend, one call site — no second embedder.

pub mod delta;
pub mod exclude;
pub mod fuse;
pub mod graph_writer;
pub mod lexical;
pub mod lock;
pub mod manifest;
pub mod purge;
pub mod search;
pub mod store;
pub mod walk;

use sha2::{Digest, Sha256};
use std::collections::HashSet;

use crate::rag::LexicalConfig;
use crate::registry::{self, WorkspaceDefinition};
use store::{IndexedChunk, RagState};

/// Result of an index pass.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct IndexOutcome {
    /// Distinct files with at least one chunk after the pass.
    pub file_count: u32,
    /// Total chunks after the pass.
    pub chunk_count: u32,
    /// A non-fatal failure (e.g. embedder unreachable) — the prior index
    /// is left intact when set.
    pub error: Option<String>,
}

/// Full index if none exists yet, otherwise a **content-hash delta**
/// ([`reindex_delta`]). A present-but-incompatible [`manifest`] (embed
/// model/dim/version changed, §3.4) forces a full rebuild so a model swap can't
/// mix incompatible vectors. The background create/activate trigger and the
/// reindex verb both call this.
pub async fn reindex(def: &WorkspaceDefinition) -> IndexOutcome {
    if store::has_index(&def.id) {
        if manifest::needs_full_rebuild(&def.id) {
            tracing::info!(
                "workspaces.rag: manifest incompatible (embed model/dim/version) — \
                 forcing full rebuild for {}",
                def.id
            );
            reindex_full(def).await
        } else {
            reindex_delta(def).await
        }
    } else {
        reindex_full(def).await
    }
}

/// Drop the existing index and re-embed every file in the folder, then write a
/// fresh content-hash [`manifest`] so subsequent passes go incremental.
pub async fn reindex_full(def: &WorkspaceDefinition) -> IndexOutcome {
    set_indexing(&def.id, true);
    let raw = walk::walk_and_chunk(&def.folder);
    // Graph-ingest alongside the vector embed: extract structural entities
    // and write Chunk/Entity nodes + typed edges. Fail-soft and fully
    // independent of the embed below (see `graph_writer`), so a sidecar or
    // graph-backend outage never blocks RAG.
    log_graph(&def.id, &graph_writer::write_graph(def, &raw).await);
    let outcome = match embed_chunks(raw).await {
        Ok(chunks) => {
            let stats = persist_full(&def.id, &chunks).await;
            tracing::info!(
                "workspaces.rag: full index of {} — {} chunks across {} files",
                def.folder,
                stats.chunk_count,
                stats.file_count
            );
            stats
        }
        Err(e) => {
            tracing::warn!(
                "workspaces.rag: full index embed failed for {}: {e}",
                def.id
            );
            // Leave any prior index untouched; surface the error.
            IndexOutcome {
                file_count: 0,
                chunk_count: 0,
                error: Some(e),
            }
        }
    };
    finish(&def.id, &outcome);
    outcome
}

/// Incremental re-index driven by the content-hash [`manifest`]: a metadata
/// walk diffs the live folder against the manifest, re-chunks+re-embeds only
/// **changed/new** files (confirmed by content hash, not just mtime), drops
/// **deleted** files' chunks, and keeps every **unchanged** vector verbatim.
/// A pre-P3 index (no manifest yet) upgrades in place via the diff's mtime
/// fallback — no mass re-embed.
pub async fn reindex_delta(def: &WorkspaceDefinition) -> IndexOutcome {
    set_indexing(&def.id, true);

    let existing = store::load_chunks(&def.id);
    let existing_files = distinct_paths(&existing);
    let existing_count = existing.len() as u32;

    // Diff: metadata walk (no content read) vs the prior manifest; hash only
    // files whose (mtime, size) drifted.
    let prior = manifest::load(&def.id).unwrap_or_else(manifest::Manifest::current_env);
    let stats = walk::walk_file_stats(&def.folder);
    let legacy = manifest::legacy_mtimes(&existing);
    let plan = manifest::diff(&prior, &stats, &legacy, |p| {
        walk::hash_file(p).map(|(h, _, _)| h)
    });

    // Chunk only the changed/new files (the cheap reads; everything unchanged
    // is never re-read).
    let mut fresh: Vec<walk::Chunk> = Vec::new();
    for p in &plan.to_embed {
        fresh.extend(walk::chunk_one_file(p));
    }
    let changed_paths: HashSet<String> = plan.to_embed.iter().cloned().collect();

    // Graph half (Ollama-independent): clear stale nodes for changed files
    // (a content change rekeys their chunk ids), drop deleted files' subtrees,
    // and re-ingest the changed/new files. MERGE-idempotent, so this converges
    // to the same graph the old whole-folder re-ingest produced — and it now
    // also prunes deletions the mtime-era delta left behind.
    apply_graph_delta(def, &fresh, &changed_paths, &plan.deleted).await;

    let reembedded = match embed_chunks(fresh).await {
        Ok(v) => v,
        Err(e) => {
            // Leave the prior on-disk index + manifest untouched (don't persist
            // a partial), but record the failure in the status.
            tracing::warn!("workspaces.rag: delta embed failed for {}: {e}", def.id);
            let outcome = IndexOutcome {
                file_count: existing_files,
                chunk_count: existing_count,
                error: Some(e),
            };
            finish(&def.id, &outcome);
            return outcome;
        }
    };

    // Merge: keep unchanged/touched chunks, add the freshly-embedded ones.
    let mut merged: Vec<IndexedChunk> = existing
        .into_iter()
        .filter(|c| plan.keep_paths.contains(&c.path))
        .collect();
    merged.extend(reembedded);

    let outcome = persist_delta(&def.id, &merged, &plan.file_meta).await;
    tracing::info!(
        "workspaces.rag: delta index of {} — {} chunks across {} files \
         ({} re-embedded, {} deleted)",
        def.folder,
        outcome.chunk_count,
        outcome.file_count,
        plan.to_embed.len(),
        plan.deleted.len(),
    );
    finish(&def.id, &outcome);
    outcome
}

/// Apply the delta's graph side: clear stale chunk nodes for changed files,
/// drop deleted files' subtrees (pruning orphan entities), then re-ingest the
/// changed/new files. Best-effort — a graph-backend outage is logged, never
/// fatal to the vector index.
async fn apply_graph_delta(
    def: &WorkspaceDefinition,
    fresh: &[walk::Chunk],
    changed_paths: &HashSet<String>,
    deleted: &[String],
) {
    let bolt = crate::graph::BoltClient::new();
    // Clear stale Chunk nodes for changed files (a new mtime rekeys the ids, so
    // a bare MERGE would orphan the old nodes). prune_orphans=false — the
    // entities are about to be re-MERGE'd.
    for p in changed_paths {
        let _ = bolt.delete_file_nodes(&def.id, p, false).await;
    }
    // Deleted files: drop the whole subtree AND prune now-orphaned entities.
    for d in deleted {
        let _ = bolt.delete_file_nodes(&def.id, d, true).await;
    }
    log_graph(&def.id, &graph_writer::write_graph(def, fresh).await);
}

/// Count of distinct file paths across a chunk set.
fn distinct_paths(chunks: &[IndexedChunk]) -> u32 {
    chunks
        .iter()
        .map(|c| c.path.as_str())
        .collect::<std::collections::HashSet<_>>()
        .len() as u32
}

/// The [`store::RagState`] snapshot for `workspace_id` (indexing flag +
/// last-index stats). The GUI polls this while a reindex is in flight.
pub fn status(workspace_id: &str) -> RagState {
    store::load_state(workspace_id)
}

/// Index `workspace_id` on a background tokio task — fire-and-forget so
/// `create` / `set_active` stay non-blocking. No-op if the workspace is
/// unknown, has RAG disabled, or has a blank folder.
pub fn spawn_background_index(workspace_id: String) {
    tokio::spawn(async move {
        let Some(def) = registry::get(&workspace_id) else {
            return;
        };
        if !def.rag_enabled || def.folder.trim().is_empty() {
            return;
        }
        let _ = reindex(&def).await;
    });
}

// ── Internal helpers ────────────────────────────────────────────────────

/// Embed a batch of walked chunks. Returns an error string (rather than
/// partial writes) so a transient embedder outage leaves the prior index
/// intact. An empty input embeds to an empty vec without an IPC round-trip.
async fn embed_chunks(raw: Vec<walk::Chunk>) -> Result<Vec<IndexedChunk>, String> {
    if raw.is_empty() {
        return Ok(Vec::new());
    }
    let texts: Vec<String> = raw.iter().map(|c| c.content.clone()).collect();
    let vectors = crate::embeddings::embed(texts)
        .await
        .map_err(|e| e.to_string())?;
    if vectors.len() != raw.len() {
        return Err(format!(
            "embedder returned {} vectors for {} chunks",
            vectors.len(),
            raw.len()
        ));
    }
    Ok(raw
        .into_iter()
        .zip(vectors)
        .map(|(c, vector)| IndexedChunk {
            id: chunk_id(&c.path, c.chunk_idx, c.mtime),
            path: c.path,
            chunk_idx: c.chunk_idx,
            content: c.content,
            mtime: c.mtime,
            start_line: c.start_line,
            end_line: c.end_line,
            vector,
        })
        .collect())
}

/// Stable per-chunk id — `sha256(path::chunk_idx::mtime)[..16]`, mirroring
/// the retired Python row id so a re-embed of an unchanged chunk is
/// idempotent.
fn chunk_id(path: &str, chunk_idx: u32, mtime: f64) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{path}::{chunk_idx}::{mtime}").as_bytes());
    hex::encode(hasher.finalize())[..16].to_owned()
}

/// Persist a **full** rebuild: write the chunks, then a fresh content-hash
/// manifest (hashing each distinct file once). Chunks-first / manifest-second,
/// under the per-workspace index lock (§3.3).
async fn persist_full(workspace_id: &str, chunks: &[IndexedChunk]) -> IndexOutcome {
    let file_count = distinct_paths(chunks);
    if registry::get(workspace_id).is_some() {
        let lock = lock::for_workspace(workspace_id);
        let _guard = lock.lock().await;
        match store::save_chunks(workspace_id, chunks) {
            Ok(()) => {
                // Chunks landed — only NOW advance the manifest (§3.3).
                if let Err(e) = manifest::save(workspace_id, &manifest::build_full(chunks)) {
                    tracing::warn!("workspaces.rag: write manifest failed for {workspace_id}: {e}");
                }
                // Lexical half (best-effort, gated on the master toggle): rebuild
                // the BM25 index from the SAME chunk slice so it can never drift
                // from chunks.jsonl. Under the index lock alongside the vector
                // pair. OFF ⇒ no-op (no lexical dir created), identity preserved.
                sync_lexical_full(workspace_id, chunks);
            }
            Err(e) => tracing::warn!("workspaces.rag: write chunks failed for {workspace_id}: {e}"),
        }
    }
    IndexOutcome {
        file_count,
        chunk_count: chunks.len() as u32,
        error: None,
    }
}

/// Rebuild the lexical (BM25) index from a chunk slice — gated on the
/// [`LexicalConfig`] master toggle, best-effort (a tantivy failure is logged,
/// never fatal to the vector index, mirroring the graph half). **OFF ⇒ no-op**,
/// so the lexical dir is never even created and retrieval stays byte-identical
/// to today. Built from the post-`ExclusionMatcher` chunk set, never a fresh
/// walk, so it inherits the index hygiene and can't drift (§2.4).
fn sync_lexical_full(workspace_id: &str, chunks: &[IndexedChunk]) {
    if !LexicalConfig::current().enabled {
        return;
    }
    if let Err(e) = lexical::build_from_chunks(workspace_id, chunks) {
        tracing::warn!("workspaces.rag.lexical: full build failed for {workspace_id}: {e}");
    }
}

/// One-time backfill (§2.5): when the toggle is ON and a workspace has chunks
/// but no `lexical/` index yet (it was indexed before lexical existed, or the
/// toggle was just flipped on), build the BM25 index once from the persisted
/// chunks — **no embedder, no Ollama** (BM25 is local), so even a 16k-chunk
/// index backfills in seconds. No-op when OFF, when the index already exists, or
/// when there are no chunks. This is what makes turning the toggle ON a true
/// switch and not a "re-index everything" event.
///
/// Best-effort + idempotent: a concurrent backfill loses the tantivy writer lock
/// and is skipped (logged), the winner builds the index. Called lazily from the
/// search path (L4) so a flip-on without a reindex still works on first query.
pub fn ensure_lexical_backfill(workspace_id: &str) {
    if !LexicalConfig::current().enabled || lexical::has_lexical_index(workspace_id) {
        return;
    }
    let chunks = store::load_chunks(workspace_id);
    if chunks.is_empty() {
        return;
    }
    tracing::info!(
        "workspaces.rag.lexical: backfilling BM25 index for {workspace_id} \
         ({} chunks, no re-embed)",
        chunks.len()
    );
    sync_lexical_full(workspace_id, &chunks);
}

/// Persist a **delta** pass: write the merged chunks, then the manifest built
/// from the diff metadata + final chunks. Chunks-first / manifest-second, under
/// the per-workspace index lock (§3.3) so a concurrent watcher delta can't tear
/// the pair.
async fn persist_delta(
    workspace_id: &str,
    chunks: &[IndexedChunk],
    file_meta: &std::collections::BTreeMap<String, (String, u64, f64)>,
) -> IndexOutcome {
    let file_count = distinct_paths(chunks);
    if registry::get(workspace_id).is_some() {
        let lock = lock::for_workspace(workspace_id);
        let _guard = lock.lock().await;
        match store::save_chunks(workspace_id, chunks) {
            Ok(()) => {
                if let Err(e) = manifest::save(workspace_id, &manifest::build(file_meta, chunks)) {
                    tracing::warn!("workspaces.rag: write manifest failed for {workspace_id}: {e}");
                }
                // Rebuild the lexical index from the merged chunk set. A folder
                // delta is not the <500ms watcher path, so a clean rebuild from
                // the authoritative merged set is the simplest convergence
                // guarantee (delta == full: both feed build_from_chunks the same
                // final chunks.jsonl). The watcher's *per-file* path stays
                // incremental (delta.rs). Gated + best-effort; OFF ⇒ no-op.
                sync_lexical_full(workspace_id, chunks);
            }
            Err(e) => tracing::warn!("workspaces.rag: write chunks failed for {workspace_id}: {e}"),
        }
    }
    IndexOutcome {
        file_count,
        chunk_count: chunks.len() as u32,
        error: None,
    }
}

/// Log the graph-write pass outcome at the level matching success/failure.
/// Graph-write is best-effort and reported separately from the embed
/// [`IndexOutcome`] — it never changes the vector index result.
fn log_graph(workspace_id: &str, g: &graph_writer::GraphOutcome) {
    if let Some(e) = &g.error {
        tracing::warn!("workspaces.rag.graph: {workspace_id} — graph-write degraded: {e}");
    } else {
        tracing::info!(
            "workspaces.rag.graph: {workspace_id} — {} chunk nodes from {} files \
             ({} skipped); edges CALLS={} IMPORTS={} INHERITS={}",
            g.chunk_nodes,
            g.files_parsed,
            g.files_skipped,
            g.calls,
            g.imports,
            g.inherits
        );
    }
}

/// Flip the indexing flag, existence-guarded.
fn set_indexing(workspace_id: &str, indexing: bool) {
    if registry::get(workspace_id).is_none() {
        return;
    }
    let mut state = store::load_state(workspace_id);
    state.indexing = indexing;
    let _ = store::save_state(workspace_id, &state);
}

/// Record the final index status (counts + last_indexed_at + error),
/// existence-guarded, and clear the indexing flag.
fn finish(workspace_id: &str, outcome: &IndexOutcome) {
    if registry::get(workspace_id).is_none() {
        return;
    }
    let mut state = store::load_state(workspace_id);
    state.indexing = false;
    state.last_error = outcome.error.clone();
    if outcome.error.is_none() {
        state.last_indexed_at = registry::epoch_now();
        state.file_count = outcome.file_count;
        state.chunk_count = outcome.chunk_count;
    }
    let _ = store::save_state(workspace_id, &state);
}

#[cfg(test)]
mod tests {
    use super::*;

    // The keep / re-embed / delete split now lives in the pure, hash-driven
    // `manifest::diff` (tested in `manifest.rs`).

    #[test]
    fn chunk_id_is_stable_and_16_hex() {
        let a = chunk_id("/a.md", 0, 1.5);
        let b = chunk_id("/a.md", 0, 1.5);
        assert_eq!(a, b, "deterministic");
        assert_eq!(a.len(), 16);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        // Different inputs → different ids.
        assert_ne!(a, chunk_id("/a.md", 1, 1.5));
        assert_ne!(a, chunk_id("/b.md", 0, 1.5));
    }

    fn idx(path: &str, idx: u32, mtime: f64) -> IndexedChunk {
        IndexedChunk {
            id: chunk_id(path, idx, mtime),
            path: path.to_owned(),
            chunk_idx: idx,
            content: "x".to_owned(),
            mtime,
            start_line: 1,
            end_line: 1,
            vector: vec![0.1, 0.2],
        }
    }

    #[tokio::test]
    async fn persist_delta_writes_chunks_then_a_matching_manifest() {
        let env = crate::test_support::TestEnv::new();
        let def = registry::create(&env.ws_path("p3"), None);
        let chunks = vec![idx("/a.rs", 0, 100.0), idx("/a.rs", 1, 100.0), idx("/b.rs", 0, 200.0)];
        let mut meta: std::collections::BTreeMap<String, (String, u64, f64)> =
            std::collections::BTreeMap::new();
        meta.insert("/a.rs".into(), ("hA".into(), 10, 100.0));
        meta.insert("/b.rs".into(), ("hB".into(), 20, 200.0));

        let out = persist_delta(&def.id, &chunks, &meta).await;
        assert_eq!(out.chunk_count, 3);
        assert_eq!(out.file_count, 2);

        // Both files exist and the manifest reflects the persisted chunks.
        assert!(store::has_index(&def.id), "chunks.jsonl written");
        let m = manifest::load(&def.id).expect("manifest written second");
        assert!(m.is_compatible());
        assert_eq!(m.files["/a.rs"].chunk_ids.len(), 2, "two chunk ids for a.rs");
        assert_eq!(m.files["/a.rs"].hash, "hA");
        assert_eq!(m.files["/b.rs"].chunk_count, 1);
    }

    #[tokio::test]
    async fn manifest_behind_chunks_converges_no_missing_vectors() {
        // Simulate a crash *between* the chunks write and the manifest write:
        // chunks are present, the manifest is stale/absent. The next diff must
        // converge — re-embed the lagging files, never skip them (§3.3).
        let env = crate::test_support::TestEnv::new();
        let def = registry::create(&env.ws_path("crash"), None);
        // Chunks landed for a.rs, but the manifest never advanced (absent).
        store::save_chunks(&def.id, &[idx("/a.rs", 0, 100.0)]).unwrap();
        assert!(manifest::load(&def.id).is_none(), "no manifest yet (crash sim)");

        // A diff with the (absent ⇒ env) manifest + the chunk's legacy mtime:
        // the unchanged file is *kept* (legacy fallback), never silently
        // skipped into a missing-vector state.
        let prior = manifest::load(&def.id).unwrap_or_else(manifest::Manifest::current_env);
        let existing = store::load_chunks(&def.id);
        let legacy = manifest::legacy_mtimes(&existing);
        let stats = vec![walk::FileStat { path: "/a.rs".into(), mtime: 100.0, size: 10 }];
        let plan = manifest::diff(&prior, &stats, &legacy, |_| Some("h".into()));
        assert!(plan.keep_paths.contains("/a.rs"), "lagging chunk kept, not lost");
        assert!(plan.to_embed.is_empty());
    }

    // ── L2: lexical full-build + backfill (gated on the master toggle) ───────

    /// Build a content-bearing chunk (the lexical index needs real tokens to
    /// score, unlike the vector-only `idx` helper above).
    fn lex_chunk(id: &str, path: &str, content: &str) -> IndexedChunk {
        IndexedChunk {
            id: id.to_owned(),
            path: path.to_owned(),
            chunk_idx: 0,
            content: content.to_owned(),
            mtime: 100.0,
            start_line: 1,
            end_line: 1,
            vector: vec![0.1, 0.2],
        }
    }

    /// Flip the process-global lexical toggle ON for the body of a test, then
    /// reset it OFF on drop so sibling tests (which share the cache) aren't left
    /// seeing it enabled. Call only inside a `TestEnv` (which holds the env lock
    /// and points `WYLDE_DATA_DIR` at a scratch dir).
    struct LexicalOn;
    impl LexicalOn {
        fn enable() -> Self {
            LexicalConfig::persist(LexicalConfig {
                enabled: true,
                ..LexicalConfig::default()
            })
            .expect("enable lexical");
            LexicalOn
        }
    }
    impl Drop for LexicalOn {
        fn drop(&mut self) {
            let _ = LexicalConfig::persist(LexicalConfig::default());
        }
    }

    #[tokio::test]
    async fn persist_full_builds_lexical_when_enabled() {
        let env = crate::test_support::TestEnv::new();
        let _on = LexicalOn::enable();
        let def = registry::create(&env.ws_path("lx-full"), None);
        let chunks = vec![
            lex_chunk("c0", "/src/search.rs", "const ANCHOR_BOOST_CAP: f64 = 0.30;"),
            lex_chunk("c1", "/src/notes.md", "prose about boosting things"),
        ];
        persist_full(&def.id, &chunks).await;
        // The lexical index was built alongside the vectors and is queryable.
        assert!(lexical::has_lexical_index(&def.id), "lexical/ built when ON");
        let hits = lexical::search(&def.id, "ANCHOR_BOOST_CAP", 5);
        assert_eq!(hits[0].0, "c0", "BM25 finds the exact token");
    }

    #[tokio::test]
    async fn persist_full_skips_lexical_when_disabled() {
        let env = crate::test_support::TestEnv::new();
        // Toggle OFF (the default) — ensure the cache is OFF for this test.
        LexicalConfig::persist(LexicalConfig::default()).unwrap();
        let def = registry::create(&env.ws_path("lx-off"), None);
        persist_full(&def.id, &[lex_chunk("c0", "/a.rs", "hello world")]).await;
        // No lexical dir is created when OFF — identity with today.
        assert!(
            !lexical::has_lexical_index(&def.id),
            "OFF ⇒ no lexical index (byte-identical to today)"
        );
    }

    #[tokio::test]
    async fn ensure_backfill_builds_from_existing_chunks_without_reembed() {
        let env = crate::test_support::TestEnv::new();
        let def = registry::create(&env.ws_path("lx-backfill"), None);
        // A pre-lexical index: chunks on disk, no lexical/ yet.
        store::save_chunks(
            &def.id,
            &[lex_chunk("c0", "/src/run_it_handler.rs", "fn run_it_handler() {}")],
        )
        .unwrap();
        assert!(!lexical::has_lexical_index(&def.id));

        // OFF ⇒ backfill is a no-op.
        LexicalConfig::persist(LexicalConfig::default()).unwrap();
        ensure_lexical_backfill(&def.id);
        assert!(!lexical::has_lexical_index(&def.id), "no backfill when OFF");

        // Flip ON ⇒ the one-time backfill builds the index from the chunks
        // already on disk (no embedder involved).
        let _on = LexicalOn::enable();
        ensure_lexical_backfill(&def.id);
        assert!(lexical::has_lexical_index(&def.id), "backfilled when ON");
        assert_eq!(lexical::search(&def.id, "run_it_handler", 5)[0].0, "c0");

        // Idempotent: a second backfill is a no-op (index already present).
        ensure_lexical_backfill(&def.id);
        assert!(lexical::has_lexical_index(&def.id));
    }
}
