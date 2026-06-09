//! In-memory symbol index + the `workspaces.symbols.find` verb (Slice F-data).
//!
//! The composer's symbol highlighting (Slice F-visual, later) queries
//! `workspaces.symbols.find` on every (debounced) keystroke, so the lookup has
//! to be cheap: a `HashMap` exact lookup is microseconds, and a fuzzy match
//! over a few thousand symbols is well under the Plan v2 §2.5 budget (<50ms;
//! the cached round-trip budget is <20ms — see §7.6's 60s client cache).
//!
//! ## Where the data comes from
//!
//! The index is built **from the code graph** — the same `Entity` nodes the
//! `workspaces.graph` verb (Slice B) reads. It reuses Slice B's
//! [`BoltClient::fetch_workspace_graph`] + [`projection::project`] read path
//! verbatim (no new Cypher, no new Bolt method), then turns the projected
//! [`Node`]s into [`SymbolEntry`]s. Consequences of the v1 graph model
//! (documented on [`projection`]):
//!
//!   * `kind` is the projection's edge-role heuristic (import endpoint →
//!     `Module`, inheritance endpoint → `Class`, else `Function`), not a
//!     stored value. A future ingest enrichment replaces it.
//!   * `line` is `0` — the graph stores no definition line yet.
//!   * Only workspace-local entities (those with a representative `file`) are
//!     indexed; synthesised external edge targets (an imported stdlib module,
//!     an inherited base never mentioned in a chunk) carry no file and are not
//!     navigable symbols, so they're excluded.
//!
//! ## Lifecycle (one workspace at a time, mirroring the watcher's MRU model)
//!
//!   * **Built at activation** — [`on_active_changed`] (called from
//!     `workspaces.set_active`) spawns a background build for the active
//!     workspace; the verb returns immediately.
//!   * **Kept fresh by the watcher** — a background subscriber to the Slice I
//!     `delta_upsert_complete` stream applies each settled per-file change via
//!     [`apply_delta`] ([`SymbolIndex::upsert`] / [`SymbolIndex::remove_file`]).
//!   * **Released on deactivation** — switching away drops the index; only the
//!     active workspace's index lives in memory.
//!
//! The auto-lifecycle hooks are gated behind [`enable`] so unit tests never
//! spawn background work; [`enable`] is called once at service boot (`main.rs`
//! → [`on_boot`]). The verb itself has an on-demand fallback: if the in-memory
//! index is absent or for a different workspace, it builds a fresh one from the
//! graph so the answer is always correct.

use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{OnceLock, RwLock};
use std::time::Instant;

use nucleo_matcher::pattern::{CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::broadcast::error::RecvError;
use wylde_shared::ipc::Reply;

use crate::error::WorkspacesError;
use crate::graph::projection::{self, Node, NodeKind, WorkspaceGraph};
use crate::graph::BoltClient;
use crate::registry;
use crate::watcher::{self, DeltaEvent};

/// Default number of matches `symbols.find` returns when no limit is given.
pub const DEFAULT_FIND_LIMIT: usize = 20;

// ── Data model ───────────────────────────────────────────────────────────────

/// One indexed symbol. `kind` reuses the graph's [`NodeKind`]; `file`/`line`
/// come straight from the projected graph node (`line` is `0` in v1), and
/// `module_path` is derived from the file path ([`module_path_for`]).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SymbolEntry {
    /// The graph's stable identifier for the symbol — the entity name, which
    /// is also the node id in the v1 graph (`projection::Node::id`).
    pub id: String,
    pub name: String,
    pub kind: NodeKind,
    pub file: PathBuf,
    pub line: u32,
    /// e.g. `wylde-harness::turn::driver` — best-effort, derived from `file`.
    pub module_path: String,
}

/// A fuzzy match: an entry plus its `0.0..=1.0` relative confidence (the top
/// match in a query scores `1.0`; the rest scale against it).
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SymbolMatch {
    pub entry: SymbolEntry,
    pub score: f32,
}

/// The `workspaces.symbols.find` reply.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct SymbolsFindResponse {
    pub query: String,
    pub matches: Vec<SymbolMatch>,
}

/// Failure building a [`SymbolIndex`].
#[derive(Debug, thiserror::Error)]
pub enum IndexError {
    /// Blank / missing workspace id.
    #[error("bad request: {0}")]
    BadRequest(String),
    /// The graph backend (Neo4j/Bolt) read failed. Carries the underlying wire
    /// `code` so the client classifier sees the same `bolt_*` string the rest
    /// of the graph surface emits.
    #[error("graph backend ({code}): {message}")]
    Backend { code: String, message: String },
}

impl From<IndexError> for WorkspacesError {
    fn from(e: IndexError) -> Self {
        match e {
            IndexError::BadRequest(m) => WorkspacesError::BadRequest(m),
            IndexError::Backend { code, message } => WorkspacesError::Backend { code, message },
        }
    }
}

/// An in-memory, name-keyed symbol index for one workspace.
///
/// `by_name` gives microsecond exact lookup; `fuzzy_keys` is the sorted set of
/// distinct names fed to the fuzzy matcher. Both are kept in sync by every
/// mutator ([`upsert`](Self::upsert) / [`remove_file`](Self::remove_file)).
#[derive(Clone, Debug)]
pub struct SymbolIndex {
    by_name: HashMap<String, Vec<SymbolEntry>>,
    fuzzy_keys: Vec<String>,
    last_built_at: Instant,
    workspace_id: String,
}

impl SymbolIndex {
    /// An empty index for `workspace_id`.
    pub fn empty(workspace_id: &str) -> Self {
        Self {
            by_name: HashMap::new(),
            fuzzy_keys: Vec::new(),
            last_built_at: Instant::now(),
            workspace_id: workspace_id.to_owned(),
        }
    }

    /// Build the index for `workspace_id` by reading its code graph live from
    /// Neo4j (Slice B's read path) and projecting the nodes into entries.
    pub async fn build(client: &BoltClient, workspace_id: &str) -> Result<Self, IndexError> {
        let ws = workspace_id.trim();
        if ws.is_empty() {
            return Err(IndexError::BadRequest("workspace_id is required".into()));
        }
        let rows = client
            .fetch_workspace_graph(ws)
            .await
            .map_err(|e| IndexError::Backend {
                code: e.code,
                message: e.message,
            })?;
        Ok(Self::from_graph(ws, &projection::project(rows)))
    }

    /// Build directly from an already-projected [`WorkspaceGraph`] — the pure
    /// half of [`build`](Self::build), used by tests and the watcher path.
    pub fn from_graph(workspace_id: &str, graph: &WorkspaceGraph) -> Self {
        let mut idx = Self::empty(workspace_id);
        idx.upsert(entries_from_graph(graph));
        idx
    }

    /// The workspace this index belongs to.
    pub fn workspace_id(&self) -> &str {
        &self.workspace_id
    }

    /// Total indexed entries (across all names).
    pub fn len(&self) -> usize {
        self.by_name.values().map(Vec::len).sum()
    }

    /// Whether the index holds no entries.
    pub fn is_empty(&self) -> bool {
        self.by_name.is_empty()
    }

    /// Exact, case-sensitive lookup. Returns every entry sharing `name` (a name
    /// can resolve to several symbols across files). `O(1)` — the hot path.
    pub fn find_exact(&self, name: &str) -> &[SymbolEntry] {
        self.by_name.get(name).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Fuzzy match `query` against every symbol name, best-first, capped at
    /// `limit` returned matches. Each matched name expands to all of its
    /// entries; scores are normalised to `0.0..=1.0` against the top hit.
    pub fn find_fuzzy(&self, query: &str, limit: usize) -> Vec<SymbolMatch> {
        let q = query.trim();
        if q.is_empty() || limit == 0 || self.fuzzy_keys.is_empty() {
            return Vec::new();
        }
        let mut matcher = Matcher::new(Config::DEFAULT);
        let pattern = Pattern::parse(q, CaseMatching::Smart, Normalization::Smart);
        // `match_list` returns `(name, score)` sorted by score descending.
        let scored: Vec<(String, u32)> =
            pattern.match_list(self.fuzzy_keys.iter().cloned(), &mut matcher);
        let top = scored.first().map(|(_, s)| *s).unwrap_or(0).max(1) as f32;

        let mut out: Vec<SymbolMatch> = Vec::with_capacity(limit.min(scored.len()));
        for (name, raw) in scored {
            let Some(entries) = self.by_name.get(&name) else {
                continue;
            };
            let score = (raw as f32 / top).clamp(0.0, 1.0);
            for entry in entries {
                out.push(SymbolMatch {
                    entry: entry.clone(),
                    score,
                });
                if out.len() >= limit {
                    return out;
                }
            }
        }
        out
    }

    /// Merge `entries` in (delta update). Idempotent per `(id, file)`: re-adding
    /// the same symbol from the same file is a no-op rather than a duplicate.
    pub fn upsert(&mut self, entries: Vec<SymbolEntry>) {
        for e in entries {
            let bucket = self.by_name.entry(e.name.clone()).or_default();
            if !bucket.iter().any(|x| x.id == e.id && x.file == e.file) {
                bucket.push(e);
            }
        }
        self.rebuild_fuzzy_keys();
        self.last_built_at = Instant::now();
    }

    /// Drop every entry whose `file` is `path` (a file was deleted or is about
    /// to be re-`upsert`ed). Names left with no entries are removed.
    pub fn remove_file(&mut self, path: &Path) {
        for bucket in self.by_name.values_mut() {
            bucket.retain(|e| e.file != path);
        }
        self.by_name.retain(|_, v| !v.is_empty());
        self.rebuild_fuzzy_keys();
        self.last_built_at = Instant::now();
    }

    /// When the index was last (re)built or mutated.
    pub fn touched_at(&self) -> Instant {
        self.last_built_at
    }

    /// Recompute the sorted distinct-name list the fuzzy matcher walks.
    fn rebuild_fuzzy_keys(&mut self) {
        let mut keys: Vec<String> = self.by_name.keys().cloned().collect();
        keys.sort_unstable();
        self.fuzzy_keys = keys;
    }

    /// Exact-first, fuzzy-fill query used by the verb: exact hits (score `1.0`)
    /// come first, then fuzzy matches not already present, capped at `limit`.
    fn query(&self, raw_query: &str, limit: usize) -> Vec<SymbolMatch> {
        let q = raw_query.trim();
        if q.is_empty() || limit == 0 {
            return Vec::new();
        }
        let mut out: Vec<SymbolMatch> = self
            .find_exact(q)
            .iter()
            .map(|e| SymbolMatch {
                entry: e.clone(),
                score: 1.0,
            })
            .collect();
        out.truncate(limit);
        if out.len() < limit {
            for m in self.find_fuzzy(q, limit) {
                if out.len() >= limit {
                    break;
                }
                let dup = out
                    .iter()
                    .any(|x| x.entry.id == m.entry.id && x.entry.file == m.entry.file);
                if !dup {
                    out.push(m);
                }
            }
        }
        out
    }
}

/// Apply a settled watcher delta to an in-memory index (pure — no I/O).
/// `new_entries` is the changed file's current symbols (empty for a removal),
/// so an edit replaces the file's old entries and a delete just drops them.
pub fn apply_delta(index: &mut SymbolIndex, path: &Path, new_entries: Vec<SymbolEntry>) {
    index.remove_file(path);
    if !new_entries.is_empty() {
        index.upsert(new_entries);
    }
}

// ── Graph → entries projection ───────────────────────────────────────────────

/// Every workspace-local symbol in `graph` (external file-less nodes excluded).
fn entries_from_graph(graph: &WorkspaceGraph) -> Vec<SymbolEntry> {
    graph
        .nodes
        .iter()
        .filter(|n| !n.file.as_os_str().is_empty())
        .map(node_to_entry)
        .collect()
}

/// The symbols whose representative `file` is `path` — the changed file's
/// current set, for a watcher delta. Inherits the projection's
/// representative-file model (an entity mentioned in several files is keyed to
/// one of them), so an edit localises to the touched file on a best-effort
/// basis; the activation rebuild is the backstop.
fn entries_for_file(graph: &WorkspaceGraph, path: &Path) -> Vec<SymbolEntry> {
    graph
        .nodes
        .iter()
        .filter(|n| n.file == path)
        .map(node_to_entry)
        .collect()
}

fn node_to_entry(n: &Node) -> SymbolEntry {
    SymbolEntry {
        id: n.id.clone(),
        name: n.name.clone(),
        kind: n.kind,
        file: n.file.clone(),
        line: n.line,
        module_path: module_path_for(&n.file),
    }
}

/// Best-effort module path from a file path, e.g.
/// `rust/crates/wylde-harness/src/turn/driver.rs` →
/// `wylde-harness::turn::driver`. Uses the crate dir just above `src` plus the
/// module tail below it; falls back to the whole path when there's no `src`.
/// Conventional roots (`mod`/`lib`/`main`) are dropped from the tail.
pub fn module_path_for(file: &Path) -> String {
    if file.as_os_str().is_empty() {
        return String::new();
    }
    let mut comps: Vec<String> = file
        .components()
        .filter_map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    if comps.is_empty() {
        return String::new();
    }
    // Strip the extension from the final component.
    if let Some(last) = comps.last_mut() {
        if let Some(stem) = Path::new(last.as_str()).file_stem() {
            *last = stem.to_string_lossy().into_owned();
        }
    }
    // Anchor on the last `src` boundary: crate dir (if any) + module tail.
    let mut parts: Vec<String> = match comps.iter().rposition(|c| c == "src") {
        Some(pos) => {
            let mut v = Vec::new();
            if pos > 0 {
                v.push(comps[pos - 1].clone());
            }
            v.extend(comps[pos + 1..].iter().cloned());
            v
        }
        None => comps,
    };
    // Drop a conventional module-root filename from the tail.
    if parts.len() > 1
        && matches!(
            parts.last().map(String::as_str),
            Some("mod" | "lib" | "main")
        )
    {
        parts.pop();
    }
    parts.join("::")
}

// ── Service verb: workspaces.symbols.find ────────────────────────────────────

/// Resolve `query` to symbols in `workspace_id`. Prefers the active in-memory
/// index; falls back to an on-demand build when the index is absent or for a
/// different workspace, so the answer is always correct. `limit` defaults to
/// [`DEFAULT_FIND_LIMIT`].
pub async fn symbols_find(
    workspace_id: &str,
    query: &str,
    limit: Option<usize>,
) -> Result<SymbolsFindResponse, WorkspacesError> {
    let ws = workspace_id.trim();
    if ws.is_empty() {
        return Err(WorkspacesError::BadRequest(
            "workspace_id is required".into(),
        ));
    }
    let limit = limit.unwrap_or(DEFAULT_FIND_LIMIT).max(1);
    let q = query.trim();

    // Fast path: the live in-memory index for this workspace.
    if let Some(matches) = query_active(ws, q, limit) {
        return Ok(SymbolsFindResponse {
            query: q.to_owned(),
            matches,
        });
    }

    // Fallback: build a fresh index from the graph (index not yet warm, or a
    // non-active workspace was asked for). Not cached as the active index —
    // the lifecycle owns that slot.
    let idx = SymbolIndex::build(&BoltClient::new(), ws).await?;
    Ok(SymbolsFindResponse {
        query: q.to_owned(),
        matches: idx.query(q, limit),
    })
}

/// `workspaces.symbols.find` action handler. Payload:
/// `{ workspace_id, query, limit? }`. Reply: [`SymbolsFindResponse`].
pub async fn handle_symbols_find(payload: Value) -> Reply {
    let ws = payload
        .get("workspace_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty());
    let Some(ws) = ws else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let query = payload.get("query").and_then(Value::as_str).unwrap_or("");
    let limit = payload
        .get("limit")
        .and_then(Value::as_u64)
        .map(|n| n as usize);

    match symbols_find(ws, query, limit).await {
        Ok(resp) => match serde_json::to_value(&resp) {
            Ok(v) => Reply::ok(v),
            Err(e) => Reply::err_msg("serde", format!("serialize symbols.find: {e}")),
        },
        Err(e) => e.to_reply(),
    }
}

// ── Process-wide lifecycle (one active workspace's index at a time) ──────────

fn index_cell() -> &'static RwLock<Option<SymbolIndex>> {
    static CELL: OnceLock<RwLock<Option<SymbolIndex>>> = OnceLock::new();
    CELL.get_or_init(|| RwLock::new(None))
}

/// Whether the auto-lifecycle hooks are armed (only the live service arms them).
static ENABLED: AtomicBool = AtomicBool::new(false);

/// Arm the activation/delta hooks. Called once by `main.rs` at service boot.
pub fn enable() {
    ENABLED.store(true, Ordering::SeqCst);
}

/// Whether [`enable`] has been called.
pub fn is_enabled() -> bool {
    ENABLED.load(Ordering::SeqCst)
}

/// Query the in-memory index iff it's present and for `ws`; `None` otherwise
/// (signals the caller to fall back to an on-demand build).
fn query_active(ws: &str, query: &str, limit: usize) -> Option<Vec<SymbolMatch>> {
    let guard = index_cell().read().ok()?;
    match guard.as_ref() {
        Some(idx) if idx.workspace_id == ws => Some(idx.query(query, limit)),
        _ => None,
    }
}

/// React to a change of active workspace: (re)build the index for the active
/// workspace in the background, or drop it when there's none. No-op until
/// [`enable`]d. Mirrors [`crate::watcher::on_active_changed`].
pub fn on_active_changed() {
    if !is_enabled() {
        return;
    }
    match registry::state::load().active_id {
        Some(id) => {
            // Already holding this workspace's index? The watcher keeps it
            // fresh — don't pay for a redundant rebuild.
            let already = index_cell()
                .read()
                .ok()
                .and_then(|g| g.as_ref().map(|i| i.workspace_id == id))
                .unwrap_or(false);
            if !already {
                spawn_build(id);
            }
        }
        None => clear(),
    }
}

/// Boot hook: arm the lifecycle, start the delta subscriber, build for the
/// active workspace (if any). Must run inside the tokio runtime.
pub fn on_boot() {
    enable();
    spawn_delta_subscriber();
    on_active_changed();
}

/// Drop the in-memory index (deactivation / shutdown). Idempotent.
pub fn clear() {
    if let Ok(mut g) = index_cell().write() {
        *g = None;
    }
}

/// Test/shutdown alias for [`clear`].
pub fn stop() {
    clear();
}

/// Spawn a background build for `ws`, replacing whatever index is held.
fn spawn_build(ws: String) {
    tokio::spawn(async move {
        match SymbolIndex::build(&BoltClient::new(), &ws).await {
            Ok(idx) => {
                let n = idx.len();
                if let Ok(mut g) = index_cell().write() {
                    *g = Some(idx);
                }
                tracing::info!("symbol_index: built {n} symbol(s) for workspace {ws}");
            }
            Err(e) => tracing::warn!("symbol_index: build failed for {ws}: {e}"),
        }
    });
}

/// Spawn the long-lived subscriber that folds watcher deltas into the index.
fn spawn_delta_subscriber() {
    tokio::spawn(async move {
        let mut rx = watcher::subscribe();
        loop {
            match rx.recv().await {
                Ok(ev) => apply_delta_event(ev).await,
                // A burst we couldn't keep up with — the next event still
                // refreshes its file; reactivation is the full backstop.
                Err(RecvError::Lagged(_)) => continue,
                Err(RecvError::Closed) => break,
            }
        }
    });
}

/// Fold one settled watcher delta into the held index (when it's the workspace
/// we currently hold). For an edit, re-derive the changed file's symbols from
/// the live graph; for a removal, just drop the file's entries.
async fn apply_delta_event(ev: DeltaEvent) {
    let is_current = index_cell()
        .read()
        .ok()
        .and_then(|g| g.as_ref().map(|i| i.workspace_id == ev.workspace_id))
        .unwrap_or(false);
    if !is_current {
        return;
    }
    let path = PathBuf::from(&ev.path);
    let new_entries = if ev.action == "remove" {
        Vec::new()
    } else {
        match BoltClient::new()
            .fetch_workspace_graph(&ev.workspace_id)
            .await
        {
            Ok(rows) => entries_for_file(&projection::project(rows), &path),
            Err(e) => {
                tracing::warn!("symbol_index: delta refetch failed for {}: {e}", ev.path);
                return;
            }
        }
    };
    if let Ok(mut g) = index_cell().write() {
        if let Some(idx) = g.as_mut().filter(|i| i.workspace_id == ev.workspace_id) {
            apply_delta(idx, &path, new_entries);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::projection::{Edge, Position, RelType};
    use std::time::Duration;

    fn entry(name: &str, file: &str, kind: NodeKind) -> SymbolEntry {
        SymbolEntry {
            id: name.to_owned(),
            name: name.to_owned(),
            kind,
            file: PathBuf::from(file),
            line: 0,
            module_path: module_path_for(Path::new(file)),
        }
    }

    fn node(name: &str, file: &str, kind: NodeKind) -> Node {
        Node {
            id: name.to_owned(),
            name: name.to_owned(),
            kind,
            file: PathBuf::from(file),
            line: 0,
            position: Position::default(),
            style: Default::default(),
        }
    }

    /// A small projected graph: three local symbols in one file, plus one
    /// file-less external edge target that must NOT be indexed.
    fn sample_graph() -> WorkspaceGraph {
        WorkspaceGraph {
            nodes: vec![
                node(
                    "parse_config",
                    "rust/crates/wylde-harness/src/config.rs",
                    NodeKind::Function,
                ),
                node(
                    "ConfigError",
                    "rust/crates/wylde-harness/src/config.rs",
                    NodeKind::Class,
                ),
                node(
                    "load",
                    "rust/crates/wylde-harness/src/config.rs",
                    NodeKind::Function,
                ),
                // external target (no file) — excluded from the index
                node("std::fs", "", NodeKind::Module),
            ],
            edges: vec![Edge {
                src: "load".into(),
                dst: "parse_config".into(),
                rel_type: RelType::Calls,
                weight: 1.0,
            }],
            clusters: vec![],
        }
    }

    #[test]
    fn build_from_graph_indexes_local_symbols_only() {
        let idx = SymbolIndex::from_graph("ws1", &sample_graph());
        assert_eq!(idx.len(), 3, "external file-less node excluded");
        assert_eq!(idx.workspace_id(), "ws1");
        // exact lookup
        let hit = idx.find_exact("parse_config");
        assert_eq!(hit.len(), 1);
        assert_eq!(hit[0].kind, NodeKind::Function);
        assert_eq!(hit[0].module_path, "wylde-harness::config");
        // the external symbol is absent
        assert!(idx.find_exact("std::fs").is_empty());
    }

    #[test]
    fn find_exact_returns_all_entries_for_a_name() {
        let mut idx = SymbolIndex::empty("ws");
        idx.upsert(vec![
            entry("run", "a/src/a.rs", NodeKind::Function),
            entry("run", "a/src/b.rs", NodeKind::Function),
        ]);
        // distinct (id,file) → two entries under the one name
        let hits = idx.find_exact("run");
        assert_eq!(hits.len(), 2, "overloads across files both kept");
        assert!(idx.find_exact("missing").is_empty());
    }

    #[test]
    fn upsert_is_idempotent_per_id_and_file() {
        let mut idx = SymbolIndex::empty("ws");
        idx.upsert(vec![entry("foo", "a/src/a.rs", NodeKind::Function)]);
        idx.upsert(vec![entry("foo", "a/src/a.rs", NodeKind::Function)]);
        assert_eq!(idx.find_exact("foo").len(), 1, "no duplicate on re-upsert");
    }

    #[test]
    fn remove_file_drops_only_that_files_entries() {
        let mut idx = SymbolIndex::empty("ws");
        idx.upsert(vec![
            entry("foo", "a/src/a.rs", NodeKind::Function),
            entry("bar", "a/src/a.rs", NodeKind::Function),
            entry("baz", "a/src/b.rs", NodeKind::Function),
        ]);
        idx.remove_file(Path::new("a/src/a.rs"));
        assert!(idx.find_exact("foo").is_empty());
        assert!(idx.find_exact("bar").is_empty());
        assert_eq!(idx.find_exact("baz").len(), 1, "other file untouched");
        assert_eq!(idx.len(), 1);
    }

    #[test]
    fn find_fuzzy_returns_real_entries_ranked() {
        let mut idx = SymbolIndex::empty("ws");
        idx.upsert(vec![
            entry("parse_config", "a/src/a.rs", NodeKind::Function),
            entry("parse_args", "a/src/a.rs", NodeKind::Function),
            entry("Renderer", "a/src/b.rs", NodeKind::Class),
        ]);
        let matches = idx.find_fuzzy("parse", 20);
        assert!(matches.len() >= 2, "both parse_* fuzzy-match: {matches:?}");
        assert!(matches.iter().all(|m| m.score > 0.0 && m.score <= 1.0));
        assert!((matches[0].score - 1.0).abs() < f32::EPSILON, "top is 1.0");
        // every returned match maps to a real entry
        assert!(matches.iter().all(|m| !m.entry.name.is_empty()));
        // an unrelated name shouldn't dominate
        assert!(matches.iter().any(|m| m.entry.name.starts_with("parse")));
    }

    #[test]
    fn find_fuzzy_limit_caps_results() {
        let mut idx = SymbolIndex::empty("ws");
        for i in 0..10 {
            idx.upsert(vec![entry(
                &format!("handler_{i}"),
                "a/src/a.rs",
                NodeKind::Function,
            )]);
        }
        let matches = idx.find_fuzzy("handler", 3);
        assert_eq!(matches.len(), 3, "limit honoured");
    }

    #[test]
    fn empty_or_whitespace_query_is_empty() {
        let idx = SymbolIndex::from_graph("ws", &sample_graph());
        assert!(idx.find_fuzzy("   ", 20).is_empty());
        assert!(idx.find_fuzzy("", 20).is_empty());
        assert!(idx.query("  ", 20).is_empty());
    }

    #[test]
    fn query_puts_exact_first_then_fuzzy() {
        let mut idx = SymbolIndex::empty("ws");
        idx.upsert(vec![
            entry("load", "a/src/a.rs", NodeKind::Function),
            entry("loader", "a/src/a.rs", NodeKind::Function),
            entry("download", "a/src/b.rs", NodeKind::Function),
        ]);
        let out = idx.query("load", 20);
        assert_eq!(out[0].entry.name, "load", "exact match leads");
        assert!((out[0].score - 1.0).abs() < f32::EPSILON);
        // fuzzy fills the rest with related names (loader, download)
        assert!(out.iter().any(|m| m.entry.name == "loader"));
        // no duplicate of the exact hit
        let loads = out.iter().filter(|m| m.entry.name == "load").count();
        assert_eq!(loads, 1);
    }

    #[test]
    fn apply_delta_replaces_file_entries() {
        let mut idx = SymbolIndex::empty("ws");
        idx.upsert(vec![entry("old_fn", "a/src/a.rs", NodeKind::Function)]);
        // an edit to a.rs that renamed old_fn → new_fn
        apply_delta(
            &mut idx,
            Path::new("a/src/a.rs"),
            vec![entry("new_fn", "a/src/a.rs", NodeKind::Function)],
        );
        assert!(idx.find_exact("old_fn").is_empty(), "old symbol gone");
        assert_eq!(idx.find_exact("new_fn").len(), 1, "new symbol present");
    }

    #[test]
    fn apply_delta_removal_drops_file() {
        let mut idx = SymbolIndex::empty("ws");
        idx.upsert(vec![
            entry("gone", "a/src/a.rs", NodeKind::Function),
            entry("kept", "a/src/b.rs", NodeKind::Function),
        ]);
        apply_delta(&mut idx, Path::new("a/src/a.rs"), Vec::new());
        assert!(idx.find_exact("gone").is_empty());
        assert_eq!(idx.find_exact("kept").len(), 1);
    }

    #[test]
    fn entries_for_file_selects_one_files_symbols() {
        let graph = sample_graph();
        let got = entries_for_file(&graph, Path::new("rust/crates/wylde-harness/src/config.rs"));
        assert_eq!(got.len(), 3);
        let none = entries_for_file(&graph, Path::new("nope.rs"));
        assert!(none.is_empty());
    }

    #[test]
    fn module_path_derivations() {
        assert_eq!(
            module_path_for(Path::new("rust/crates/wylde-harness/src/turn/driver.rs")),
            "wylde-harness::turn::driver"
        );
        assert_eq!(
            module_path_for(Path::new("rust/crates/wylde-harness/src/lib.rs")),
            "wylde-harness"
        );
        assert_eq!(
            module_path_for(Path::new("C:/ws/src/widget.rs")),
            "ws::widget"
        );
        assert_eq!(module_path_for(Path::new("a/b/c.rs")), "a::b::c");
        assert_eq!(module_path_for(Path::new("widget.rs")), "widget");
        assert_eq!(module_path_for(Path::new("")), "");
    }

    #[test]
    fn touched_at_advances_on_mutation() {
        let mut idx = SymbolIndex::empty("ws");
        let t0 = idx.touched_at();
        idx.upsert(vec![entry("x", "a/src/a.rs", NodeKind::Function)]);
        assert!(idx.touched_at() >= t0);
    }

    #[tokio::test]
    async fn handle_symbols_find_requires_workspace_id() {
        let r = handle_symbols_find(serde_json::json!({})).await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn symbols_find_rejects_blank_workspace() {
        let err = symbols_find("  ", "x", None).await.unwrap_err();
        assert_eq!(err.code(), "bad_request");
    }

    // ── Perf budgets (Plan v2 §2.5) ──────────────────────────────────────────

    fn synthetic_index(n: usize) -> SymbolIndex {
        let mut idx = SymbolIndex::empty("perf");
        let entries: Vec<SymbolEntry> = (0..n)
            .map(|i| {
                let name = format!("symbol_{i:05}");
                let file = format!("crate/src/mod_{}.rs", i / 50);
                entry(&name, &file, NodeKind::Function)
            })
            .collect();
        idx.upsert(entries);
        idx
    }

    #[test]
    fn perf_exact_lookup_is_sub_millisecond() {
        let idx = synthetic_index(5000);
        assert_eq!(idx.len(), 5000);
        let name = "symbol_02500";
        // Warm + measure a batch so timer resolution doesn't dominate.
        let start = Instant::now();
        for _ in 0..1000 {
            assert!(!idx.find_exact(name).is_empty());
        }
        let per = start.elapsed() / 1000;
        eprintln!("[perf] exact per-lookup = {per:?}");
        assert!(
            per < Duration::from_millis(1),
            "exact lookup too slow: {per:?}"
        );
    }

    #[test]
    fn perf_fuzzy_match_under_50ms_on_5k() {
        let idx = synthetic_index(5000);
        let start = Instant::now();
        let matches = idx.find_fuzzy("sym2500", 20);
        let elapsed = start.elapsed();
        eprintln!("[perf] fuzzy 5k = {elapsed:?} ({} matches)", matches.len());
        assert!(!matches.is_empty(), "expected fuzzy hits");
        assert!(
            elapsed < Duration::from_millis(50),
            "fuzzy too slow: {elapsed:?}"
        );
    }

    // ── Watcher-delta subscriber fold (the `delta_upsert_complete` path) ─────
    //
    // These drive the process-global index cell, so they serialise on a shared
    // guard and reset the cell on the way out. The `upsert` action's refetch
    // needs live Neo4j (covered by the #[ignore] integration test); the gate +
    // `remove` path are fully exercisable in-process.

    async fn cell_guard() -> tokio::sync::MutexGuard<'static, ()> {
        static LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
        LOCK.lock().await
    }

    fn install_index(idx: SymbolIndex) {
        *index_cell().write().unwrap() = Some(idx);
    }
    fn reset_cell() {
        *index_cell().write().unwrap() = None;
    }
    fn delta(ws: &str, path: &str, action: &'static str) -> DeltaEvent {
        DeltaEvent {
            workspace_id: ws.to_owned(),
            path: path.to_owned(),
            action,
            graph_chunk_nodes: 0,
            took_ms: 1.0,
        }
    }

    #[tokio::test]
    async fn delta_remove_event_updates_the_held_index() {
        let _g = cell_guard().await;
        install_index(SymbolIndex::from_graph("wsX", &sample_graph()));
        let file = "rust/crates/wylde-harness/src/config.rs";

        apply_delta_event(delta("wsX", file, "remove")).await;

        let guard = index_cell().read().unwrap();
        let idx = guard.as_ref().expect("index held");
        assert!(
            idx.find_exact("parse_config").is_empty(),
            "removed file's symbols gone"
        );
        assert!(idx.find_exact("load").is_empty());
        drop(guard);
        reset_cell();
    }

    #[tokio::test]
    async fn delta_event_for_another_workspace_is_ignored() {
        let _g = cell_guard().await;
        install_index(SymbolIndex::from_graph("wsA", &sample_graph()));

        // A delta for wsB must not touch wsA's held index.
        apply_delta_event(delta(
            "wsB",
            "rust/crates/wylde-harness/src/config.rs",
            "remove",
        ))
        .await;

        let guard = index_cell().read().unwrap();
        assert_eq!(
            guard.as_ref().unwrap().find_exact("parse_config").len(),
            1,
            "other-workspace delta ignored"
        );
        drop(guard);
        reset_cell();
    }

    #[tokio::test]
    async fn lifecycle_clear_drops_the_index() {
        let _g = cell_guard().await;
        install_index(SymbolIndex::from_graph("wsC", &sample_graph()));
        clear();
        assert!(
            index_cell().read().unwrap().is_none(),
            "clear drops the index"
        );
        reset_cell();
    }
}
