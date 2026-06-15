//! [`WorkspaceMemoryEntry`] — one curated workspace-layer note.
//!
//! Stored as one JSON object per line in
//! `<data_dir>/workspaces/<workspace_id>/memory.jsonl` (Q5 layout). This
//! is the workspace tier of the 3-layer memory model — narrower than
//! long-term (global) memory, broader than short-term (per-conversation
//! working memory).
//!
//! Relocated from the harness `workspaces::memory::entry` (Slice 0c) so the
//! workspace notes tier is owned by the `wylde-workspaces` service. The
//! on-disk path is **byte-identical** to the harness original (same
//! `<data_dir>/workspaces/<id>/memory.jsonl` via [`crate::registry`]), so
//! existing user notes are picked up unchanged — no migration needed for the
//! notes tier (the only path shift in 0c is conversations).
//!
//! JSONL (rather than a single JSON array) lets the curation layer append a
//! new insight without rewriting the whole file. Reads tolerate a
//! torn/missing file and skip unparseable lines.

use serde::{Deserialize, Serialize};

/// A single workspace-scoped note (the "memory entry" of the workspace
/// tier). The Build Order struct index calls this `NoteEntry`; the proven
/// on-disk field set is preserved verbatim (so existing `memory.jsonl` files
/// load unchanged) and [`NoteEntry`] is a type alias onto it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct WorkspaceMemoryEntry {
    /// Stable entry id.
    pub id: String,

    /// The remembered fact / insight (the text injected into the
    /// prompt).
    pub text: String,

    /// Provenance tag — where this note came from. Empty for notes the
    /// curation / reflection layer minted; set to `"long-term-copy"` when
    /// the user manually promotes a long-term memory into this workspace
    /// (the C2b copy-in opt-in). Defaults empty so existing `memory.jsonl`
    /// files load unchanged.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub source: String,

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

/// Build Order alias (struct index, §Appendix B). The canonical type keeps
/// the proven field set for byte-identical on-disk compatibility; later
/// slices that reshape the schema can migrate behind this name.
pub type NoteEntry = WorkspaceMemoryEntry;

impl WorkspaceMemoryEntry {
    /// Build a fresh entry (no embedding yet). The id is a short,
    /// sortable timestamp-based token; callers that want relevance
    /// scoring populate `embedding` via [`super::query::embed_text`].
    pub fn new(id: impl Into<String>, text: impl Into<String>) -> Self {
        let now = crate::registry::epoch_now();
        Self {
            id: id.into(),
            text: text.into(),
            source: String::new(),
            created_at: now,
            last_used_at: now,
            embedding: Vec::new(),
        }
    }

    /// Turn this entry into the wire shape the `workspaces.notes.*` verbs
    /// return (the embedding is internal-only and omitted).
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::json!({
            "id": self.id,
            "text": self.text,
            "source": self.source,
            "created_at": self.created_at,
            "last_used_at": self.last_used_at,
        })
    }
}

/// Mint a sortable, unique note id: `note-<epoch_nanos>-<counter>`. The
/// epoch-nanos prefix keeps ids time-ordered; the process-local atomic
/// counter disambiguates two adds inside the same nanosecond. No external
/// crate (the service has no `chrono` / `rand` dep — the conversation store
/// reuses ids minted upstream, so id generation only matters here).
pub fn new_note_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    format!("note-{nanos:039}-{seq:08x}")
}

/// `<data_dir>/workspaces/<workspace_id>/memory.jsonl`.
pub fn memory_path(workspace_id: &str) -> std::path::PathBuf {
    crate::registry::persistence::workspace_dir(workspace_id).join("memory.jsonl")
}

/// Load every entry for a workspace (fail-soft: empty on missing/torn;
/// unparseable lines are skipped).
pub fn load(workspace_id: &str) -> Vec<WorkspaceMemoryEntry> {
    let Ok(raw) = wylde_shared::encryption::read_to_string_at_rest(&memory_path(workspace_id))
    else {
        return Vec::new();
    };
    raw.lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| serde_json::from_str::<WorkspaceMemoryEntry>(l).ok())
        .collect()
}

/// Encrypt-at-rest (OI-14) + atomically replace a workspace's `memory.jsonl`.
pub fn save(workspace_id: &str, entries: &[WorkspaceMemoryEntry]) -> std::io::Result<()> {
    let path = crate::registry::persistence::workspace_dir(workspace_id).join("memory.jsonl");
    let mut body = String::new();
    for e in entries {
        body.push_str(&serde_json::to_string(e).unwrap());
        body.push('\n');
    }
    wylde_shared::encryption::write_at_rest(&path, body.as_bytes())
}

/// Append one entry (load → push → save). Convenience for the curation
/// layer; not on the hot path.
pub fn append(workspace_id: &str, entry: WorkspaceMemoryEntry) -> std::io::Result<()> {
    let mut all = load(workspace_id);
    all.push(entry);
    save(workspace_id, &all)
}

/// Replace the `text` (and re-stamp `last_used_at`) of an existing entry,
/// clearing its cached embedding so the caller re-embeds the new text.
/// Returns the updated entry, or `None` if no entry has that id.
pub fn update_text(
    workspace_id: &str,
    id: &str,
    text: &str,
) -> std::io::Result<Option<WorkspaceMemoryEntry>> {
    let mut all = load(workspace_id);
    let Some(idx) = all.iter().position(|e| e.id == id) else {
        return Ok(None);
    };
    all[idx].text = text.to_owned();
    all[idx].last_used_at = crate::registry::epoch_now();
    all[idx].embedding = Vec::new();
    let updated = all[idx].clone();
    save(workspace_id, &all)?;
    Ok(Some(updated))
}

/// Persist a single entry's new embedding in place (used after an add /
/// update once the text has been embedded). No-op if the id is gone.
pub fn set_embedding(workspace_id: &str, id: &str, embedding: Vec<f32>) -> std::io::Result<()> {
    let mut all = load(workspace_id);
    if let Some(e) = all.iter_mut().find(|e| e.id == id) {
        e.embedding = embedding;
        save(workspace_id, &all)?;
    }
    Ok(())
}

/// Remove the entry with `id`. Returns `true` iff an entry was removed.
pub fn delete(workspace_id: &str, id: &str) -> std::io::Result<bool> {
    let all = load(workspace_id);
    let before = all.len();
    let kept: Vec<WorkspaceMemoryEntry> = all.into_iter().filter(|e| e.id != id).collect();
    if kept.len() == before {
        return Ok(false);
    }
    save(workspace_id, &kept)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestEnv;

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
        // Verify it really is line-delimited (decrypt-at-rest first: the file
        // is ciphertext on disk under OI-14).
        let raw = wylde_shared::encryption::read_to_string_at_rest(&memory_path(ws)).unwrap();
        assert_eq!(raw.lines().filter(|l| !l.is_empty()).count(), 2);
    }

    #[test]
    fn source_tag_round_trips_and_defaults_empty() {
        let _env = TestEnv::new();
        let ws = "ws-source-000000";
        let tagged = WorkspaceMemoryEntry {
            source: "long-term-copy".to_owned(),
            ..WorkspaceMemoryEntry::new("s1", "promoted insight")
        };
        append(ws, tagged).unwrap();
        // A plain note minted via `new` carries no source.
        append(ws, WorkspaceMemoryEntry::new("s2", "ambient note")).unwrap();
        let back = load(ws);
        assert_eq!(back[0].source, "long-term-copy");
        assert!(back[1].source.is_empty(), "default source is empty");
        // Pre-source on-disk lines (no `source` key) still load.
        let dir = crate::registry::persistence::workspace_dir("ws-source-legacy-000000");
        crate::common::ensure_dir(&dir).unwrap();
        wylde_shared::encryption::write_at_rest(
            &memory_path("ws-source-legacy-000000"),
            b"{\"id\":\"old\",\"text\":\"legacy\"}\n",
        )
        .unwrap();
        let legacy = load("ws-source-legacy-000000");
        assert_eq!(legacy.len(), 1);
        assert!(legacy[0].source.is_empty());
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
        let dir = crate::registry::persistence::workspace_dir(ws);
        crate::common::ensure_dir(&dir).unwrap();
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

    #[test]
    fn update_text_replaces_and_clears_embedding() {
        let _env = TestEnv::new();
        let ws = "ws-upd-000000";
        append(
            ws,
            WorkspaceMemoryEntry {
                embedding: vec![1.0, 2.0],
                ..WorkspaceMemoryEntry::new("x", "old")
            },
        )
        .unwrap();
        let updated = update_text(ws, "x", "new").unwrap().expect("found");
        assert_eq!(updated.text, "new");
        assert!(updated.embedding.is_empty(), "embedding cleared on edit");
        // Persisted.
        assert_eq!(load(ws)[0].text, "new");
        // Unknown id → None.
        assert!(update_text(ws, "ghost", "z").unwrap().is_none());
    }

    #[test]
    fn set_embedding_persists_in_place() {
        let _env = TestEnv::new();
        let ws = "ws-emb-000000";
        append(ws, WorkspaceMemoryEntry::new("x", "t")).unwrap();
        set_embedding(ws, "x", vec![0.5, 0.5]).unwrap();
        assert_eq!(load(ws)[0].embedding, vec![0.5, 0.5]);
        // Unknown id is a silent no-op.
        set_embedding(ws, "ghost", vec![1.0]).unwrap();
    }

    #[test]
    fn delete_removes_and_reports_truthfully() {
        let _env = TestEnv::new();
        let ws = "ws-del-000000";
        append(ws, WorkspaceMemoryEntry::new("a", "one")).unwrap();
        append(ws, WorkspaceMemoryEntry::new("b", "two")).unwrap();
        assert!(delete(ws, "a").unwrap());
        assert_eq!(load(ws).len(), 1);
        assert_eq!(load(ws)[0].id, "b");
        // Second delete of the same id is a no-op false.
        assert!(!delete(ws, "a").unwrap());
    }
}
