//! On-disk store for the per-workspace file index.
//!
//! **Storage (Q5 clean break):** everything lives under the workspace's
//! own bundle at `<data_dir>/workspaces/<id>/index/`, NOT a separate
//! top-level `indexes/` root the retired Python LanceDB indexer used:
//!
//! ```text
//! <data_dir>/workspaces/<id>/index/
//! ├── chunks.jsonl     # one IndexedChunk per line (vector + metadata)
//! └── rag_state.json   # RagState: indexing flag + last-index stats
//! ```
//!
//! ## Why JSONL + brute-force, not LanceDB
//!
//! The retired Python indexer stored vectors in a LanceDB table. The
//! `lancedb` Rust crate does NOT build cleanly on Windows — it needs an
//! external `protoc` toolchain and pulls ~515 transitive crates — and the
//! cross-language-read benefit is moot now that the Python LanceDB
//! indexer is deleted (clean break). A per-workspace folder holds at most
//! low-thousands of chunks, so a linear cosine scan beats an ANN index's
//! build/query overhead — the same rationale (and JSONL idiom) the
//! workspace-memory tier (`memory.jsonl` + `query::cosine`) and the
//! long-term vector store already use. See `search.rs` for the scan.
//!
//! Atomic-write discipline matches the rest of the harness memory layer:
//! reads tolerate a missing/torn file by returning empty/default, writes
//! go to `<path>.tmp` then rename.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::common::ensure_dir;
use crate::registry::persistence::workspace_dir;

/// One embedded chunk persisted to `chunks.jsonl`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct IndexedChunk {
    /// Stable id: `sha256(path::chunk_idx::mtime)[..16]` — mirrors the
    /// retired Python row id so re-embeds are idempotent per (file, chunk,
    /// mtime).
    pub id: String,
    /// Absolute source-file path.
    pub path: String,
    /// 0-based chunk index within its file.
    pub chunk_idx: u32,
    /// Chunk text.
    pub content: String,
    /// Source-file mtime (epoch seconds) the chunk was embedded at.
    pub mtime: f64,
    /// 1-based first line of the chunk in its source file.
    pub start_line: u32,
    /// 1-based last line of the chunk in its source file.
    pub end_line: u32,
    /// The embedding vector.
    pub vector: Vec<f32>,
}

/// Per-workspace index status, polled by the GUI while a reindex runs.
///
/// This is the redesign's home for the indexing flag (the retired
/// `memory::workspaces::store::Workspace.indexing` field has no analogue
/// on the config-only [`crate::registry::WorkspaceDefinition`],
/// so the live status lives here next to the data it describes).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Default)]
pub struct RagState {
    /// True while a full/delta index pass is in flight.
    pub indexing: bool,
    /// Epoch seconds of the last completed index, or 0.0 if never.
    pub last_indexed_at: f64,
    /// Distinct files with at least one chunk.
    pub file_count: u32,
    /// Total chunks across all files.
    pub chunk_count: u32,
    /// Last failure message (e.g. embedder unreachable), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Live progress of the in-flight pass (phase / counts / rate / ETA),
    /// present only while `indexing` is true. Joined onto each `list_mru` row
    /// so the GUI renders a real progress bar + ETA instead of a bare
    /// "Indexing…". Additive + `Option`, so a not-indexing state and older
    /// readers are unaffected. See [`super::progress::IndexProgress`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub progress: Option<super::progress::IndexProgress>,
}

impl RagState {
    /// Serialize to a `serde_json::Value` for the IPC layer / GUI.
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

/// `<data_dir>/workspaces/<id>/index/`.
pub fn index_dir(workspace_id: &str) -> PathBuf {
    workspace_dir(workspace_id).join("index")
}

fn chunks_path(workspace_id: &str) -> PathBuf {
    index_dir(workspace_id).join("chunks.jsonl")
}

fn state_path(workspace_id: &str) -> PathBuf {
    index_dir(workspace_id).join("rag_state.json")
}

/// True when an index already exists for this workspace (decides full vs
/// delta on a reindex).
pub fn has_index(workspace_id: &str) -> bool {
    chunks_path(workspace_id).exists()
}

/// Load every persisted chunk. Returns empty on a missing file; skips
/// individual torn lines rather than failing the whole read.
pub fn load_chunks(workspace_id: &str) -> Vec<IndexedChunk> {
    let raw = match std::fs::read_to_string(chunks_path(workspace_id)) {
        Ok(r) => r,
        Err(_) => return Vec::new(),
    };
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<IndexedChunk>(l).ok())
        .collect()
}

/// Atomically write the full chunk set to `chunks.jsonl`, creating the
/// `index/` dir if needed.
pub fn save_chunks(workspace_id: &str, chunks: &[IndexedChunk]) -> std::io::Result<()> {
    let dir = index_dir(workspace_id);
    ensure_dir(&dir)?;
    let path = chunks_path(workspace_id);
    let tmp = path.with_extension("jsonl.tmp");
    let mut body = String::new();
    for c in chunks {
        // One JSON object per line. `to_string` (not pretty) keeps each
        // record on a single line so the file stays valid JSONL.
        body.push_str(&serde_json::to_string(c).unwrap_or_default());
        body.push('\n');
    }
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Load the index status. Returns the default (`indexing: false`, counts
/// 0) on a missing/torn file.
pub fn load_state(workspace_id: &str) -> RagState {
    std::fs::read_to_string(state_path(workspace_id))
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default()
}

/// Atomically write the index status.
pub fn save_state(workspace_id: &str, state: &RagState) -> std::io::Result<()> {
    let dir = index_dir(workspace_id);
    ensure_dir(&dir)?;
    let path = state_path(workspace_id);
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(state).unwrap())?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestEnv;

    fn sample(id: &str, path: &str) -> IndexedChunk {
        IndexedChunk {
            id: id.to_owned(),
            path: path.to_owned(),
            chunk_idx: 0,
            content: "hello".to_owned(),
            mtime: 1.0,
            start_line: 1,
            end_line: 1,
            vector: vec![0.1, 0.2, 0.3],
        }
    }

    #[test]
    fn save_then_load_chunks_round_trips() {
        let _env = TestEnv::new();
        let ws = "ws-aaa";
        let chunks = vec![sample("id1", "/a.md"), sample("id2", "/b.md")];
        save_chunks(ws, &chunks).unwrap();
        assert!(has_index(ws));
        let back = load_chunks(ws);
        assert_eq!(back, chunks);
    }

    #[test]
    fn load_chunks_absent_is_empty() {
        let _env = TestEnv::new();
        assert!(load_chunks("nope-000000").is_empty());
        assert!(!has_index("nope-000000"));
    }

    #[test]
    fn state_round_trips_and_defaults() {
        let _env = TestEnv::new();
        let ws = "ws-state";
        assert_eq!(load_state(ws), RagState::default());
        let st = RagState {
            indexing: false,
            last_indexed_at: 123.0,
            file_count: 2,
            chunk_count: 5,
            last_error: None,
            progress: None,
        };
        save_state(ws, &st).unwrap();
        assert_eq!(load_state(ws), st);
    }

    #[test]
    fn save_chunks_skips_torn_lines_on_load() {
        let _env = TestEnv::new();
        let ws = "ws-torn";
        save_chunks(ws, &[sample("id1", "/a.md")]).unwrap();
        // Append a garbage line — load must skip it, keep the good one.
        let p = index_dir(ws).join("chunks.jsonl");
        let mut body = std::fs::read_to_string(&p).unwrap();
        body.push_str("{ not json\n");
        std::fs::write(&p, body).unwrap();
        assert_eq!(load_chunks(ws).len(), 1);
    }
}
