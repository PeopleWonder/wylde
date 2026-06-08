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

pub mod graph_writer;
pub mod search;
pub mod store;
pub mod walk;

use sha2::{Digest, Sha256};
use std::collections::HashMap;

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

/// Full index if none exists yet, otherwise an mtime delta. The
/// background create/activate trigger and the reindex verb both call this.
pub async fn reindex(def: &WorkspaceDefinition) -> IndexOutcome {
    if store::has_index(&def.id) {
        reindex_delta(def).await
    } else {
        reindex_full(def).await
    }
}

/// Drop the existing index and re-embed every file in the folder.
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
            let stats = persist(&def.id, &chunks);
            tracing::info!(
                "workspaces.rag: full index of {} — {} chunks across {} files",
                def.folder,
                stats.chunk_count,
                stats.file_count
            );
            stats
        }
        Err(e) => {
            tracing::warn!("workspaces.rag: full index embed failed for {}: {e}", def.id);
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

/// Re-embed only files whose mtime is newer than the cached chunk; drop
/// chunks for files that disappeared from the folder.
pub async fn reindex_delta(def: &WorkspaceDefinition) -> IndexOutcome {
    set_indexing(&def.id, true);

    let existing = store::load_chunks(&def.id);
    let existing_files = distinct_paths(&existing);
    let existing_count = existing.len() as u32;

    let walked = walk::walk_and_chunk(&def.folder);
    // Re-ingest the graph for the full current folder each pass — `upsert`
    // / `relate` MERGE, so this is idempotent. (Stale-node pruning on file
    // delete is a future-slice concern; `delete_workspace` covers cleanup.)
    log_graph(&def.id, &graph_writer::write_graph(def, &walked).await);
    let plan = plan_delta(existing, walked);

    let reembedded = match embed_chunks(plan.to_embed).await {
        Ok(v) => v,
        Err(e) => {
            // Leave the prior on-disk index untouched (don't persist a
            // partial), but record the failure in the status.
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

    let mut merged = plan.keep;
    merged.extend(reembedded);

    let outcome = persist(&def.id, &merged);
    tracing::info!(
        "workspaces.rag: delta index of {} — {} chunks across {} files",
        def.folder,
        outcome.chunk_count,
        outcome.file_count
    );
    finish(&def.id, &outcome);
    outcome
}

/// The unchanged-keep / needs-embed split a delta pass computes from the
/// current on-disk chunks and a fresh folder walk. Pure — no IO — so the
/// mtime selection logic is unit-testable without a live embedder.
struct DeltaPlan {
    /// Existing chunks for files still present AND unchanged — kept as-is.
    keep: Vec<IndexedChunk>,
    /// Freshly-walked chunks for new or mtime-changed files — to embed.
    to_embed: Vec<walk::Chunk>,
}

/// Decide which existing chunks survive and which walked chunks need
/// re-embedding. A file is "changed" when its walked mtime is newer than
/// the newest cached chunk for that path (1 ms tolerance); a file is
/// "gone" when it has cached chunks but no walked chunk — its chunks are
/// dropped by being excluded from `keep`.
fn plan_delta(existing: Vec<IndexedChunk>, walked: Vec<walk::Chunk>) -> DeltaPlan {
    // path -> max cached mtime.
    let mut cached_mtime: HashMap<String, f64> = HashMap::new();
    for c in &existing {
        let e = cached_mtime.entry(c.path.clone()).or_insert(c.mtime);
        if c.mtime > *e {
            *e = c.mtime;
        }
    }

    let mut live_paths: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut to_embed: Vec<walk::Chunk> = Vec::new();
    for ch in walked {
        live_paths.insert(ch.path.clone());
        match cached_mtime.get(&ch.path) {
            // Unchanged within tolerance — keep the cached chunk instead.
            Some(m) if *m >= ch.mtime - 0.001 => {}
            _ => to_embed.push(ch),
        }
    }
    let changed_paths: std::collections::HashSet<String> =
        to_embed.iter().map(|c| c.path.clone()).collect();

    let keep: Vec<IndexedChunk> = existing
        .into_iter()
        .filter(|c| live_paths.contains(&c.path) && !changed_paths.contains(&c.path))
        .collect();

    DeltaPlan { keep, to_embed }
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

/// Write the merged chunk set (existence-guarded) and return the file /
/// chunk counts.
fn persist(workspace_id: &str, chunks: &[IndexedChunk]) -> IndexOutcome {
    let file_count = distinct_paths(chunks);
    // Existence-guard: if the workspace was deleted mid-index (the
    // background task can outlive a `workspaces.delete`), do NOT recreate
    // its bundle dir.
    if registry::get(workspace_id).is_some() {
        if let Err(e) = store::save_chunks(workspace_id, chunks) {
            tracing::warn!("workspaces.rag: write chunks failed for {workspace_id}: {e}");
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

    fn chunk_of(path: &str, mtime: f64) -> IndexedChunk {
        IndexedChunk {
            id: chunk_id(path, 0, mtime),
            path: path.to_owned(),
            chunk_idx: 0,
            content: "old".to_owned(),
            mtime,
            start_line: 1,
            end_line: 1,
            vector: vec![1.0, 0.0],
        }
    }

    fn walked_of(path: &str, mtime: f64) -> walk::Chunk {
        walk::Chunk {
            path: path.to_owned(),
            chunk_idx: 0,
            content: "new".to_owned(),
            mtime,
            start_line: 1,
            end_line: 1,
        }
    }

    #[test]
    fn plan_delta_keeps_unchanged_reembeds_changed_and_new_drops_gone() {
        let existing = vec![
            chunk_of("/a.md", 100.0), // unchanged
            chunk_of("/b.md", 100.0), // will be changed (newer walk)
            chunk_of("/c.md", 100.0), // gone (not in walk)
        ];
        let walked = vec![
            walked_of("/a.md", 100.0), // same mtime → keep cached
            walked_of("/b.md", 200.0), // newer → re-embed
            walked_of("/d.md", 50.0),  // new file → embed
        ];
        let plan = plan_delta(existing, walked);

        let kept: Vec<&str> = plan.keep.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(kept, vec!["/a.md"], "only unchanged-and-live survives");

        let embed: std::collections::HashSet<&str> =
            plan.to_embed.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(
            embed,
            ["/b.md", "/d.md"].into_iter().collect(),
            "changed + new get re-embedded; gone /c.md dropped"
        );
    }

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
}
