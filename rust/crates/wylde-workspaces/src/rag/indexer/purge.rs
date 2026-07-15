//! One-time filter-only purge of an already-polluted index (index hygiene P2).
//!
//! The walk-time [`exclude`](super::exclude) fix keeps **new** indexes clean,
//! but an index built before it keeps its build-artifact chunks (R4 measured
//! ~58 % — the `target-dev/doc` rustdoc tree) until something rewrites
//! `chunks.jsonl`. This drops exactly those chunks **without re-embedding**:
//! the surviving vectors are kept verbatim, so the purge is fast and never
//! touches Ollama.
//!
//! It is deliberately *filter-only* — re-clustering the concepts over the
//! cleaned set is a separate, explicit step (`workspaces.concepts.build_semantic`)
//! the caller runs after, so the concept set is re-derived from real source.
//!
//! Graph-clean is best-effort: the dropped files' Chunk nodes are removed from
//! Neo4j when the backend is reachable (collapsed to the topmost excluded
//! ancestor dir so a 1 000-file rustdoc tree is a handful of subtree deletes,
//! not 1 000 calls), and skipped (logged) when it isn't — it never blocks the
//! vector-side win.

use std::collections::BTreeSet;
use std::path::Path;

use serde::Serialize;

use crate::graph::BoltClient;
use crate::registry::{self, WorkspaceDefinition};

use super::exclude::ExclusionMatcher;
use super::{lock, manifest, store};

/// What a purge did, for the verb reply + the migration log line.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct PurgeOutcome {
    /// Chunks before the purge.
    pub before: u32,
    /// Chunks dropped (matcher-excluded).
    pub dropped: u32,
    /// Chunks kept (the surviving real-source index).
    pub kept: u32,
    /// Distinct file paths whose chunks were dropped.
    pub files_dropped: u32,
    /// Excluded chunks still present after the rewrite — the verification
    /// invariant: must be 0.
    pub excluded_remaining: u32,
    /// Subtree graph-deletes that succeeded (best-effort).
    pub graph_cleaned: u32,
    /// First graph-clean failure (backend unreachable), if any — non-fatal.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub graph_error: Option<String>,
}

impl PurgeOutcome {
    /// As the IPC reply `Value`.
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

/// Drop every persisted chunk whose path the [`ExclusionMatcher`] now excludes,
/// rewrite `chunks.jsonl` + the index counts, and best-effort graph-clean the
/// dropped files. No re-embed. Idempotent — a second run drops nothing.
pub async fn purge_excluded(def: &WorkspaceDefinition) -> PurgeOutcome {
    let matcher = ExclusionMatcher::for_root(Path::new(&def.folder));
    let chunks = store::load_chunks(&def.id);
    let before = chunks.len() as u32;

    let mut kept = Vec::with_capacity(chunks.len());
    let mut dropped_files: BTreeSet<String> = BTreeSet::new();
    let mut dropped = 0u32;
    for c in chunks {
        if matcher.is_excluded(Path::new(&c.path), false) {
            dropped += 1;
            dropped_files.insert(c.path.clone());
        } else {
            kept.push(c);
        }
    }

    let mut outcome = PurgeOutcome {
        before,
        dropped,
        kept: kept.len() as u32,
        files_dropped: dropped_files.len() as u32,
        excluded_remaining: 0,
        graph_cleaned: 0,
        graph_error: None,
    };

    if dropped == 0 {
        tracing::info!(
            "workspaces.rag.purge: {} already clean ({before} chunks, 0 excluded)",
            def.id
        );
        return outcome;
    }

    // Verification invariant — nothing excluded should survive the filter.
    outcome.excluded_remaining = kept
        .iter()
        .filter(|c| matcher.is_excluded(Path::new(&c.path), false))
        .count() as u32;

    // Existence-guard: a delete that raced the purge must not recreate the
    // bundle dir (same discipline as `persist_full`).
    if registry::get(&def.id).is_some() {
        let lk = lock::for_workspace(&def.id);
        let _g = lk.lock().await;
        if let Err(e) = store::save_chunks(&def.id, &kept) {
            tracing::warn!(
                "workspaces.rag.purge: write chunks failed for {}: {e}",
                def.id
            );
            // Couldn't persist — report the would-be result without touching
            // state, so the caller sees the failure rather than a false win.
            return outcome;
        }
        // Manifest second (§3.3): drop the purged files' entries, keeping the
        // surviving files' hashes/ids verbatim (the purge changes no content).
        // A legacy index with no manifest is left as-is — the next reindex
        // writes one.
        if let Some(mut m) = manifest::load(&def.id) {
            let kept_paths: std::collections::HashSet<&str> =
                kept.iter().map(|c| c.path.as_str()).collect();
            m.files.retain(|p, _| kept_paths.contains(p.as_str()));
            let _ = manifest::save(&def.id, &m);
        }
        let mut st = store::load_state(&def.id);
        st.file_count = super::distinct_paths(&kept);
        st.chunk_count = kept.len() as u32;
        let _ = store::save_state(&def.id, &st);
    }

    // Best-effort graph-clean, collapsed to topmost excluded ancestor dirs.
    let keys = collapse_to_excluded_ancestors(&matcher, Path::new(&def.folder), &dropped_files);
    let bolt = BoltClient::new();
    for key in &keys {
        let reply = bolt.delete_file_nodes(&def.id, key, true).await;
        if reply.ok {
            outcome.graph_cleaned += 1;
        } else {
            // Backend unreachable — record the first error and stop hammering
            // it (the vector-side win already landed above).
            if outcome.graph_error.is_none() {
                outcome.graph_error = reply
                    .error
                    .map(|e| format!("{} {}", e.code, e.message))
                    .or_else(|| Some("graph-clean failed".to_owned()));
            }
            break;
        }
    }

    tracing::info!(
        "workspaces.rag.purge: {} — dropped {dropped}/{before} chunks across {} files \
         (kept {}, excluded_remaining {}, graph subtrees cleaned {}{})",
        def.id,
        outcome.files_dropped,
        outcome.kept,
        outcome.excluded_remaining,
        outcome.graph_cleaned,
        outcome
            .graph_error
            .as_deref()
            .map(|e| format!(", graph_err={e}"))
            .unwrap_or_default(),
    );
    outcome
}

/// Collapse a set of dropped file paths to the **topmost excluded ancestor
/// directory** of each, so an excluded subtree (e.g. `…/target-dev/`) becomes a
/// single subtree-delete key instead of one per file. A file excluded on its
/// own merit (a `*.min.js` under a kept dir) stays itself.
fn collapse_to_excluded_ancestors(
    matcher: &ExclusionMatcher,
    root: &Path,
    files: &BTreeSet<String>,
) -> BTreeSet<String> {
    let root_len = root.to_string_lossy().len();
    let mut keys: BTreeSet<String> = BTreeSet::new();
    for f in files {
        let mut key = f.clone();
        let mut cur = Path::new(f).to_path_buf();
        while let Some(parent) = cur.parent() {
            // Never climb to/above the workspace root.
            if parent.to_string_lossy().len() <= root_len {
                break;
            }
            if matcher.is_excluded(parent, true) {
                key = parent.to_string_lossy().into_owned();
                cur = parent.to_path_buf();
            } else {
                break;
            }
        }
        keys.insert(key);
    }
    // Drop any key that is itself under another key (the subtree-delete on the
    // ancestor already covers it).
    keys.iter()
        .filter(|k| {
            !keys.iter().any(|other| {
                other.as_str() != k.as_str()
                    && k.starts_with(&format!("{other}{}", std::path::MAIN_SEPARATOR))
            })
        })
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestEnv;
    use store::IndexedChunk;

    fn chunk(path: &str) -> IndexedChunk {
        IndexedChunk {
            id: format!("id-{path}"),
            path: path.to_owned(),
            chunk_idx: 0,
            content: "x".to_owned(),
            mtime: 1.0,
            start_line: 1,
            end_line: 1,
            vector: vec![0.1, 0.2],
        }
    }

    #[tokio::test]
    async fn purge_drops_only_excluded_chunks() {
        let env = TestEnv::new();
        // A real registered workspace whose folder is a tempdir root.
        let folder = env.ws_path("proj");
        std::fs::create_dir_all(&folder).unwrap();
        let def = registry::create(&folder, None);
        let root = folder.replace('/', std::path::MAIN_SEPARATOR_STR);

        // Mix real source with target-dev rustdoc + node_modules artifacts.
        let join = |rel: &str| format!("{root}{}{rel}", std::path::MAIN_SEPARATOR);
        let chunks = vec![
            chunk(&join("rust/src/main.rs")),
            chunk(&join("rust/target-dev/doc/x.html")),
            chunk(&join("rust/target-dev/doc/y.html")),
            chunk(&join("Core/GUI/target-dev/doc/z.html")),
            chunk(&join("node_modules/dep/index.js")),
            chunk(&join("docs/readme.md")),
        ];
        store::save_chunks(&def.id, &chunks).unwrap();

        let out = purge_excluded(&def).await;
        assert_eq!(out.before, 6);
        assert_eq!(out.dropped, 4, "the 3 rustdoc + 1 node_modules chunks");
        assert_eq!(out.kept, 2, "main.rs + readme.md survive");
        assert_eq!(out.excluded_remaining, 0);

        // Persisted store reflects the filter.
        let left: Vec<String> = store::load_chunks(&def.id)
            .into_iter()
            .map(|c| c.path)
            .collect();
        assert!(left.iter().any(|p| p.ends_with("main.rs")));
        assert!(left.iter().any(|p| p.ends_with("readme.md")));
        assert!(!left.iter().any(|p| p.contains("target-dev")));
        assert!(!left.iter().any(|p| p.contains("node_modules")));

        // Counts updated.
        let st = store::load_state(&def.id);
        assert_eq!(st.chunk_count, 2);
        assert_eq!(st.file_count, 2);
    }

    #[tokio::test]
    async fn purge_is_idempotent_on_a_clean_index() {
        let env = TestEnv::new();
        let folder = env.ws_path("clean");
        std::fs::create_dir_all(&folder).unwrap();
        let def = registry::create(&folder, None);
        let root = folder.replace('/', std::path::MAIN_SEPARATOR_STR);
        let join = |rel: &str| format!("{root}{}{rel}", std::path::MAIN_SEPARATOR);
        store::save_chunks(
            &def.id,
            &[chunk(&join("src/a.rs")), chunk(&join("src/b.rs"))],
        )
        .unwrap();

        let out = purge_excluded(&def).await;
        assert_eq!(out.dropped, 0);
        assert_eq!(out.kept, 2);
        // A second run is still a no-op.
        let out2 = purge_excluded(&def).await;
        assert_eq!(out2.dropped, 0);
        assert_eq!(out2.kept, 2);
    }

    #[cfg(windows)]
    #[test]
    fn collapse_groups_a_subtree_into_one_key() {
        let root = Path::new(r"C:\ws");
        let m = ExclusionMatcher::for_root(root);
        let sep = std::path::MAIN_SEPARATOR;
        let mut files = BTreeSet::new();
        files.insert(r"C:\ws\rust\target-dev\doc\x.html".to_owned());
        files.insert(r"C:\ws\rust\target-dev\doc\y.html".to_owned());
        files.insert(r"C:\ws\Core\GUI\target-dev\doc\z.html".to_owned());
        let keys = collapse_to_excluded_ancestors(&m, root, &files);
        // Both rust/target-dev html files collapse to one target-dev key; the
        // GUI one to its own. Two subtree keys, not three files.
        assert_eq!(keys.len(), 2, "got {keys:?}");
        assert!(keys
            .iter()
            .all(|k| k.ends_with("target-dev") || k.contains(&format!("target-dev{sep}"))));
    }
}
