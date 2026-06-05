//! [`WorkspaceMemoryEntry`] — one curated workspace-layer memory item.
//!
//! Stored as one JSON object per line in
//! `<data_dir>/workspaces/<workspace_id>/memory.jsonl` (Q5 layout). This
//! is the workspace tier of the 3-layer memory model — narrower than
//! long-term (global) memory, broader than short-term (per-conversation
//! working memory).
//!
//! JSONL (rather than a single JSON array) lets the curation layer
//! append a new insight without rewriting the whole file. Reads tolerate
//! a torn/missing file and skip unparseable lines.

use serde::{Deserialize, Serialize};

use crate::memory::common::ensure_dir;

/// A single workspace-scoped memory entry.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceMemoryEntry {
    /// Stable entry id.
    pub id: String,

    /// The remembered fact / insight (the text injected into the
    /// prompt).
    pub text: String,

    /// Creation time (epoch seconds).
    #[serde(default)]
    pub created_at: f64,

    /// Last time this entry was surfaced into a prompt — feeds the
    /// recency component of [`super::query`] scoring.
    #[serde(default)]
    pub last_used_at: f64,

    /// Cached `nomic-embed-text` embedding of `text` (768d), computed on
    /// write so per-turn relevance scoring is one query-embed + a dot
    /// product rather than N embeds. Empty when not yet embedded — the
    /// entry then contributes recency only (relevance term → 0).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub embedding: Vec<f32>,
}

impl WorkspaceMemoryEntry {
    /// Build a fresh entry (no embedding yet). The id is a short,
    /// sortable timestamp-based token; callers that want relevance
    /// scoring populate `embedding` via [`super::query::embed_text`].
    pub fn new(id: impl Into<String>, text: impl Into<String>) -> Self {
        let now = super::super::registry::epoch_now();
        Self {
            id: id.into(),
            text: text.into(),
            created_at: now,
            last_used_at: now,
            embedding: Vec::new(),
        }
    }
}

/// `<data_dir>/workspaces/<workspace_id>/memory.jsonl`.
pub fn memory_path(workspace_id: &str) -> std::path::PathBuf {
    super::super::registry::persistence::workspace_dir(workspace_id).join("memory.jsonl")
}

/// Load every entry for a workspace (fail-soft: empty on missing/torn;
/// unparseable lines are skipped).
pub fn load(workspace_id: &str) -> Vec<WorkspaceMemoryEntry> {
    let Ok(raw) = std::fs::read_to_string(memory_path(workspace_id)) else {
        return Vec::new();
    };
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<WorkspaceMemoryEntry>(l).ok())
        .collect()
}

/// Atomically replace a workspace's `memory.jsonl`.
pub fn save(workspace_id: &str, entries: &[WorkspaceMemoryEntry]) -> std::io::Result<()> {
    let dir = super::super::registry::persistence::workspace_dir(workspace_id);
    ensure_dir(&dir)?;
    let path = dir.join("memory.jsonl");
    let mut body = String::new();
    for e in entries {
        body.push_str(&serde_json::to_string(e).unwrap());
        body.push('\n');
    }
    let tmp = path.with_extension("jsonl.tmp");
    std::fs::write(&tmp, body)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

/// Append one entry (load → push → save). Convenience for the curation
/// layer; not on the hot path.
pub fn append(workspace_id: &str, entry: WorkspaceMemoryEntry) -> std::io::Result<()> {
    let mut all = load(workspace_id);
    all.push(entry);
    save(workspace_id, &all)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspaces::test_support::TestEnv;

    #[test]
    fn save_then_load_round_trips_jsonl() {
        let _env = TestEnv::new();
        let ws = "ws-test-000000";
        let entries = vec![
            WorkspaceMemoryEntry::new("e1", "first insight"),
            WorkspaceMemoryEntry {
                embedding: vec![0.1, 0.2, 0.3],
                ..WorkspaceMemoryEntry::new("e2", "second insight")
            },
        ];
        save(ws, &entries).unwrap();
        let back = load(ws);
        assert_eq!(back, entries);
        // Verify it really is line-delimited.
        let raw = std::fs::read_to_string(memory_path(ws)).unwrap();
        assert_eq!(raw.lines().filter(|l| !l.is_empty()).count(), 2);
    }

    #[test]
    fn load_is_empty_for_missing_file() {
        let _env = TestEnv::new();
        assert!(load("nope-000000").is_empty());
    }

    #[test]
    fn load_skips_torn_lines() {
        let _env = TestEnv::new();
        let ws = "ws-torn-000000";
        let dir = super::super::super::registry::persistence::workspace_dir(ws);
        ensure_dir(&dir).unwrap();
        std::fs::write(
            memory_path(ws),
            "{not json}\n{\"id\":\"ok\",\"text\":\"good\"}\n",
        )
        .unwrap();
        let back = load(ws);
        assert_eq!(back.len(), 1);
        assert_eq!(back[0].id, "ok");
    }

    #[test]
    fn append_adds_to_existing() {
        let _env = TestEnv::new();
        let ws = "ws-append-000000";
        append(ws, WorkspaceMemoryEntry::new("a", "one")).unwrap();
        append(ws, WorkspaceMemoryEntry::new("b", "two")).unwrap();
        assert_eq!(load(ws).len(), 2);
    }
}
