//! Workspace registry — `Workspace` struct, JSON store, MRU bookkeeping,
//! delete. Rust port of `Core/harness/memory/workspaces/_store.py`'s
//! registry surface (the indexing half lands in slice 7.B).
//!
//! ## Storage layout
//!
//! Mirrors Python exactly:
//!
//! * `<data_dir>/workspaces.json` — MRU-ordered list of `Workspace`s.
//! * `<data_dir>/indexes/<slug>/` — per-workspace LanceDB folder. 7.A
//!   only manages the *folder*; the LanceDB content lands in 7.B.
//! * `<data_dir>/workspace_memories/<slug>/` — durable workspace
//!   memory (LLM-curated insights). Survives MRU eviction; only
//!   explicit `delete_workspace` removes it.
//!
//! ## Concurrency
//!
//! Python guards the registry with `threading.RLock()`. Rust uses a
//! process-wide `parking_lot`-style `Mutex` (we use `std::sync::Mutex`
//! to avoid pulling in a new crate). Holds are short — JSON IO is
//! synchronous and small.

use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::slug::slug_for;
use crate::memory::common::{data_dir, ensure_dir};

static REGISTRY_LOCK: Mutex<()> = Mutex::new(());

/// `<data_dir>/indexes`. Per-workspace `<slug>/` LanceDB folders live
/// underneath. Created lazily — first writer creates parents.
pub fn indexes_dir() -> PathBuf {
    data_dir().join("indexes")
}

/// `<data_dir>/workspaces.json`. Top-level shape: `{"workspaces": [...]}`.
pub fn registry_path() -> PathBuf {
    data_dir().join("workspaces.json")
}

/// `<data_dir>/workspace_memories`. Per-workspace `<slug>/` folder
/// holds the durable LLM-curated memory. Used only by `delete_workspace`
/// in 7.A; slice 7.C will own the read/write surface.
fn workspace_memories_dir() -> PathBuf {
    data_dir().join("workspace_memories")
}

// ── Workspace dataclass ────────────────────────────────────────────────

/// Mirrors Python's `Workspace` dataclass exactly. JSON shape on the
/// wire matches — fields are serialised with the same snake_case names
/// and the same numeric types.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Workspace {
    pub id: String,
    pub path: String,
    #[serde(default)]
    pub persona: String,
    #[serde(default)]
    pub file_count: u64,
    #[serde(default)]
    pub last_indexed_at: f64,
    #[serde(default)]
    pub last_activated_at: f64,
    /// True while a refresh / reindex is mid-flight. 7.A never sets
    /// this to true — only the 7.B indexer does. Carried so the JSON
    /// round-trip preserves the field when 7.B writes it.
    #[serde(default)]
    pub indexing: bool,
}

impl Workspace {
    /// Build a fresh workspace from a path. The slug is derived
    /// deterministically; timestamps stay 0.0 until activation.
    pub fn new(path: &str) -> Self {
        Self {
            id: slug_for(path),
            path: path.to_owned(),
            persona: String::new(),
            file_count: 0,
            last_indexed_at: 0.0,
            last_activated_at: 0.0,
            indexing: false,
        }
    }

    /// Convert to a `serde_json::Value`. Mirrors Python's `to_dict()`
    /// for callers that hand the result straight to the IPC layer.
    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).unwrap_or(Value::Null)
    }
}

// ── Registry IO ────────────────────────────────────────────────────────

/// Read the registry JSON. Returns an empty list on any error so a
/// torn file doesn't brick the surface — matches Python semantics.
pub fn load_registry() -> Vec<Workspace> {
    let path = registry_path();
    if !path.exists() {
        return Vec::new();
    }
    let raw = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    parse_registry_json(&raw)
}

/// Parse the JSON shape `{"workspaces": [...]}`. Also accepts a bare
/// `[...]` to match Python's `items = raw.get("workspaces") if
/// isinstance(raw, dict) else raw` fallback.
fn parse_registry_json(raw: &str) -> Vec<Workspace> {
    let Ok(v): Result<Value, _> = serde_json::from_str(raw) else {
        return Vec::new();
    };
    let items: &Value = if v.is_object() {
        v.get("workspaces").unwrap_or(&Value::Null)
    } else {
        &v
    };
    let Some(arr) = items.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|it| serde_json::from_value::<Workspace>(it.clone()).ok())
        .collect()
}

/// Atomically replace the registry. Writes to `<path>.tmp` then
/// renames — mirrors Python's `with_suffix(".json.tmp").replace(path)`.
pub fn save_registry(workspaces: &[Workspace]) -> std::io::Result<()> {
    let path = registry_path();
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    let payload = serde_json::json!({"workspaces": workspaces});
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&payload).unwrap())?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

// ── Public read surface ────────────────────────────────────────────────

/// Workspaces in MRU order (most recent first).
pub fn list_workspaces() -> Vec<Workspace> {
    let _g = REGISTRY_LOCK.lock().unwrap();
    load_registry()
}

/// First N workspaces in MRU order. `limit` defaults to the
/// user-configured MRU cap (see `mru::get_mru_limit`).
pub fn recent_workspaces(limit: Option<u64>) -> Vec<Workspace> {
    let n = match limit {
        Some(v) => v as usize,
        None => super::mru::get_mru_limit() as usize,
    };
    let mut all = list_workspaces();
    if all.len() > n {
        all.truncate(n);
    }
    all
}

/// Lookup by workspace id. Returns `None` if no match.
pub fn get_workspace(workspace_id: &str) -> Option<Workspace> {
    let _g = REGISTRY_LOCK.lock().unwrap();
    load_registry().into_iter().find(|w| w.id == workspace_id)
}

// ── Activation bookkeeping (registry side only) ────────────────────────

/// Move a workspace to the head of the MRU list (or insert it if new),
/// update `last_activated_at`, persist, and evict the tail past the
/// MRU cap. **Does not run indexing** — that's the 7.B half; the
/// returned `Workspace` carries `file_count: 0` for a freshly-minted
/// entry.
///
/// Returns `Err` if `path` doesn't exist or isn't a directory.
pub fn touch_activated(path: &str) -> Result<Workspace, ActivationError> {
    let folder = PathBuf::from(path);
    let metadata = std::fs::metadata(&folder).map_err(|_| ActivationError::NotFound {
        path: path.to_owned(),
    })?;
    if !metadata.is_dir() {
        return Err(ActivationError::NotADirectory {
            path: path.to_owned(),
        });
    }

    let abs = std::fs::canonicalize(&folder)
        .ok()
        .map(|p| {
            let s = p.to_string_lossy();
            if let Some(stripped) = s.strip_prefix(r"\\?\") {
                PathBuf::from(stripped)
            } else {
                p
            }
        })
        .unwrap_or(folder);
    let abs_str = abs.to_string_lossy().into_owned();

    let _g = REGISTRY_LOCK.lock().unwrap();
    let mut workspaces = load_registry();
    let slug = slug_for(&abs_str);
    let existing_idx = workspaces.iter().position(|w| w.id == slug);

    let mut entry = match existing_idx {
        Some(i) => workspaces.remove(i),
        None => Workspace::new(&abs_str),
    };
    entry.last_activated_at = epoch_now();

    workspaces.insert(0, entry.clone());
    let _ = save_registry(&workspaces);

    let limit = super::mru::get_mru_limit() as usize;
    let evicted = evict_past_mru(&mut workspaces, limit);
    if !evicted.is_empty() {
        let _ = save_registry(&workspaces);
    }

    Ok(entry)
}

/// Errors raised by [`touch_activated`].
#[derive(Debug, thiserror::Error)]
pub enum ActivationError {
    #[error("workspace path does not exist: {path}")]
    NotFound { path: String },
    #[error("workspace path is not a directory: {path}")]
    NotADirectory { path: String },
}

/// Trim `workspaces` to `limit`, removing index dirs for evicted
/// entries. Workspace memory is preserved so a re-activated workspace
/// lands warm. Returns the list of evicted ids.
///
/// Mutates in place. Caller is responsible for `save_registry`.
pub fn evict_past_mru(workspaces: &mut Vec<Workspace>, limit: usize) -> Vec<String> {
    let mut evicted = Vec::new();
    while workspaces.len() > limit {
        let victim = workspaces.pop().unwrap();
        evicted.push(victim.id.clone());
        delete_index_dir(&victim.id);
    }
    evicted
}

fn delete_index_dir(workspace_id: &str) {
    let target = indexes_dir().join(workspace_id);
    if !target.exists() {
        return;
    }
    let _ = std::fs::remove_dir_all(&target);
}

fn delete_memory_dir(workspace_id: &str) {
    let target = workspace_memories_dir().join(workspace_id);
    if !target.exists() {
        return;
    }
    let _ = std::fs::remove_dir_all(&target);
}

/// Explicit user delete. Removes from the registry, deletes the
/// per-workspace index folder, AND deletes the durable workspace
/// memory folder. Returns `false` if the workspace wasn't in the
/// registry.
pub fn delete_workspace(workspace_id: &str) -> bool {
    let _g = REGISTRY_LOCK.lock().unwrap();
    let mut workspaces = load_registry();
    let before = workspaces.len();
    workspaces.retain(|w| w.id != workspace_id);
    if workspaces.len() == before {
        return false;
    }
    let _ = save_registry(&workspaces);

    delete_index_dir(workspace_id);
    delete_memory_dir(workspace_id);
    true
}

// ── Persona ────────────────────────────────────────────────────────────

/// Persist a new persona for a workspace. Returns `false` if the
/// workspace wasn't in the registry.
pub fn set_persona(workspace_id: &str, text: &str) -> bool {
    let _g = REGISTRY_LOCK.lock().unwrap();
    let mut workspaces = load_registry();
    let mut hit = false;
    for w in workspaces.iter_mut() {
        if w.id == workspace_id {
            w.persona = text.to_owned();
            hit = true;
        }
    }
    if hit {
        let _ = save_registry(&workspaces);
    }
    hit
}

/// Read the persona text for a workspace. Returns `""` if the
/// workspace doesn't exist — mirrors Python's `get_persona`.
pub fn get_persona(workspace_id: &str) -> String {
    get_workspace(workspace_id)
        .map(|w| w.persona)
        .unwrap_or_default()
}

// ── Indexer-side helpers (7.B will call these) ─────────────────────────

/// Flip the `indexing` flag on the registry entry. When `flag` becomes
/// false we also bump `last_indexed_at` to now — matches Python's
/// `_set_indexing`.
pub fn set_indexing_flag(workspace_id: &str, flag: bool) {
    let _g = REGISTRY_LOCK.lock().unwrap();
    let mut workspaces = load_registry();
    let mut hit = false;
    for w in workspaces.iter_mut() {
        if w.id == workspace_id {
            w.indexing = flag;
            if !flag {
                w.last_indexed_at = epoch_now();
            }
            hit = true;
        }
    }
    if hit {
        let _ = save_registry(&workspaces);
    }
}

/// Update `file_count` on the registry entry. Mirrors Python's
/// `_update_workspace_metadata`.
pub fn update_file_count(workspace_id: &str, file_count: u64) {
    let _g = REGISTRY_LOCK.lock().unwrap();
    let mut workspaces = load_registry();
    let mut hit = false;
    for w in workspaces.iter_mut() {
        if w.id == workspace_id {
            w.file_count = file_count;
            hit = true;
        }
    }
    if hit {
        let _ = save_registry(&workspaces);
    }
}

// ── Utilities ──────────────────────────────────────────────────────────

fn epoch_now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::workspaces::test_support::TestEnv;
    use tempfile::tempdir;

    #[test]
    fn workspace_roundtrips_through_json() {
        let w = Workspace {
            id: "proj-abc123".into(),
            path: "/tmp/proj".into(),
            persona: "talk like a pirate".into(),
            file_count: 42,
            last_indexed_at: 1.0,
            last_activated_at: 2.0,
            indexing: false,
        };
        let s = serde_json::to_string(&w).unwrap();
        let back: Workspace = serde_json::from_str(&s).unwrap();
        assert_eq!(w, back);
    }

    #[test]
    fn save_and_load_round_trip_preserves_order() {
        let _env = TestEnv::new();
        let entries = vec![Workspace::new("/tmp/a"), Workspace::new("/tmp/b")];
        save_registry(&entries).unwrap();
        let back = load_registry();
        assert_eq!(back.len(), 2);
        assert_eq!(back[0].path, "/tmp/a");
        assert_eq!(back[1].path, "/tmp/b");
    }

    #[test]
    fn load_registry_returns_empty_for_missing_file() {
        let _env = TestEnv::new();
        assert!(load_registry().is_empty());
    }

    #[test]
    fn load_registry_recovers_from_torn_json() {
        let _env = TestEnv::new();
        if let Some(parent) = registry_path().parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(registry_path(), "{not json").unwrap();
        assert!(load_registry().is_empty());
    }

    #[test]
    fn parse_registry_accepts_bare_array() {
        let raw = r#"[{"id":"x","path":"/p","persona":"","file_count":0,
                       "last_indexed_at":0.0,"last_activated_at":0.0,"indexing":false}]"#;
        let ws = parse_registry_json(raw);
        assert_eq!(ws.len(), 1);
        assert_eq!(ws[0].id, "x");
    }

    #[test]
    fn touch_activated_moves_existing_to_head() {
        let _env = TestEnv::new();
        let td = tempdir().unwrap();
        let path_a = td.path().join("a");
        let path_b = td.path().join("b");
        std::fs::create_dir(&path_a).unwrap();
        std::fs::create_dir(&path_b).unwrap();

        touch_activated(path_a.to_str().unwrap()).unwrap();
        touch_activated(path_b.to_str().unwrap()).unwrap();
        // A should be at the tail, B at head.
        let ws = list_workspaces();
        assert_eq!(ws.len(), 2);
        assert!(ws[0].path.ends_with("b"), "got {:?}", ws[0].path);

        // Re-activate A — it must move to head.
        touch_activated(path_a.to_str().unwrap()).unwrap();
        let ws = list_workspaces();
        assert!(ws[0].path.ends_with("a"), "got {:?}", ws[0].path);
        assert!(ws[1].path.ends_with("b"), "got {:?}", ws[1].path);
    }

    #[test]
    fn touch_activated_errors_on_missing_path() {
        let _env = TestEnv::new();
        // Use a path that genuinely doesn't exist on either OS.
        let err = touch_activated(
            std::env::temp_dir()
                .join("no-such-wylde-test-dir-honest-12345")
                .to_str()
                .unwrap(),
        )
        .unwrap_err();
        match err {
            ActivationError::NotFound { .. } => {}
            other => panic!("expected NotFound, got {other:?}"),
        }
    }

    #[test]
    fn touch_activated_errors_when_path_is_a_file() {
        let _env = TestEnv::new();
        let td = tempdir().unwrap();
        let f = td.path().join("a.txt");
        std::fs::write(&f, b"x").unwrap();
        let err = touch_activated(f.to_str().unwrap()).unwrap_err();
        match err {
            ActivationError::NotADirectory { .. } => {}
            other => panic!("expected NotADirectory, got {other:?}"),
        }
    }

    #[test]
    fn evict_past_mru_drops_tail_and_returns_evicted_ids() {
        let _env = TestEnv::new();
        let mut ws = (0..7)
            .map(|i| Workspace::new(&format!("/tmp/p{i}")))
            .collect::<Vec<_>>();
        let evicted = evict_past_mru(&mut ws, 5);
        assert_eq!(ws.len(), 5);
        assert_eq!(evicted.len(), 2);
        // Tail-most evicted first → ids correspond to original positions
        // 6 then 5.
        assert!(evicted[0].starts_with("p6-"));
        assert!(evicted[1].starts_with("p5-"));
    }

    #[test]
    fn delete_workspace_removes_registry_and_disk() {
        let _env = TestEnv::new();
        let td = tempdir().unwrap();
        let p = td.path().join("d");
        std::fs::create_dir(&p).unwrap();
        let w = touch_activated(p.to_str().unwrap()).unwrap();
        // Seed an index dir + memory dir so delete has something to remove.
        let idx = indexes_dir().join(&w.id);
        let mem = workspace_memories_dir().join(&w.id);
        std::fs::create_dir_all(&idx).unwrap();
        std::fs::create_dir_all(&mem).unwrap();
        assert!(idx.exists() && mem.exists());

        assert!(delete_workspace(&w.id));
        assert!(get_workspace(&w.id).is_none());
        assert!(!idx.exists(), "index dir should be gone");
        assert!(!mem.exists(), "workspace memory dir should be gone");
    }

    #[test]
    fn delete_workspace_returns_false_for_unknown_id() {
        let _env = TestEnv::new();
        assert!(!delete_workspace("nope-000000"));
    }

    #[test]
    fn persona_set_then_get_returns_what_was_saved() {
        let _env = TestEnv::new();
        let td = tempdir().unwrap();
        let p = td.path().join("p");
        std::fs::create_dir(&p).unwrap();
        let w = touch_activated(p.to_str().unwrap()).unwrap();
        assert!(set_persona(&w.id, "say only the word 'aye'"));
        assert_eq!(get_persona(&w.id), "say only the word 'aye'");
    }

    #[test]
    fn persona_set_returns_false_for_unknown_id() {
        let _env = TestEnv::new();
        assert!(!set_persona("nope-000000", "x"));
    }

    #[test]
    fn set_indexing_flag_clears_and_bumps_last_indexed_at() {
        let _env = TestEnv::new();
        let td = tempdir().unwrap();
        let p = td.path().join("i");
        std::fs::create_dir(&p).unwrap();
        let w = touch_activated(p.to_str().unwrap()).unwrap();
        set_indexing_flag(&w.id, true);
        let mid = get_workspace(&w.id).unwrap();
        assert!(mid.indexing);

        set_indexing_flag(&w.id, false);
        let end = get_workspace(&w.id).unwrap();
        assert!(!end.indexing);
        assert!(end.last_indexed_at > 0.0);
    }

    #[test]
    fn update_file_count_persists_value() {
        let _env = TestEnv::new();
        let td = tempdir().unwrap();
        let p = td.path().join("u");
        std::fs::create_dir(&p).unwrap();
        let w = touch_activated(p.to_str().unwrap()).unwrap();
        update_file_count(&w.id, 17);
        assert_eq!(get_workspace(&w.id).unwrap().file_count, 17);
    }

    #[test]
    fn recent_workspaces_respects_explicit_limit() {
        let _env = TestEnv::new();
        let td = tempdir().unwrap();
        for i in 0..3 {
            let p = td.path().join(format!("r{i}"));
            std::fs::create_dir(&p).unwrap();
            touch_activated(p.to_str().unwrap()).unwrap();
        }
        let two = recent_workspaces(Some(2));
        assert_eq!(two.len(), 2);
    }
}
