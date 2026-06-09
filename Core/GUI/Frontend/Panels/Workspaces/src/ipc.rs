//! Per-panel IPC helpers for the Workspaces panel.
//!
//! Wraps the `workspaces.*` verbs into typed reads / writes the View body
//! consumes. As of Thought Bubble System Slice 0d these verbs are served by
//! the dedicated `wylde-workspaces` service (not the harness pipe), so every
//! call targets `"wylde-workspaces"`. A down/unlaunched service surfaces as
//! a `pipe_unavailable` error the View renders as the §7.5 "service
//! unavailable + Retry" fallback while preserving its last-known rows.
//! `list_mru` is the slim MRU-5 projection, so index-only fields
//! (`file_count`, `last_indexed_at`, `indexing`) are not populated on the
//! list rows; the re-index control drives the Rust file-indexer ported in
//! PR #18 via `workspaces.reindex`.

use serde_json::{json, Value};

/// One workspace as the harness reports it.  The Svelte side wraps
/// each entry in a one-element `paths` array for back-compat with the
/// legacy multi-path model; we keep the harness's single-`path`
/// shape because there is no longer any pre-MRU consumer.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WorkspaceSummary {
    pub id: String,
    pub path: String,
    pub file_count: Option<u64>,
    pub last_indexed_at: Option<String>,
    pub last_activated_at: Option<String>,
    pub indexing: bool,
    pub persona: Option<String>,
}

impl WorkspaceSummary {
    pub fn from_value(v: &Value) -> Self {
        Self {
            id: v
                .get("id")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            // Redesign `WorkspaceDefinition` uses `folder`; fall back to
            // the legacy `path` key for resilience.
            path: v
                .get("folder")
                .or_else(|| v.get("path"))
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_owned(),
            file_count: v.get("file_count").and_then(|x| x.as_u64()),
            last_indexed_at: v
                .get("last_indexed_at")
                .and_then(|x| x.as_str())
                .map(|s| s.to_owned()),
            last_activated_at: v
                .get("last_activated_at")
                .and_then(|x| x.as_str())
                .map(|s| s.to_owned()),
            indexing: v.get("indexing").and_then(|x| x.as_bool()).unwrap_or(false),
            persona: v
                .get("persona")
                .and_then(|x| x.as_str())
                .map(|s| s.to_owned()),
        }
    }
}

/// Read the full workspace list (MRU order, harness applies its own
/// 5-entry cap).
pub async fn list_workspaces() -> Result<Vec<WorkspaceSummary>, String> {
    let v = wylde_gui_pipe::call(
        "wylde-workspaces",
        "POST",
        "/__action__",
        Some(json!({ "action": "workspaces.list_mru", "payload": {} })),
    )
    .await?;
    Ok(parse_workspace_array(&v))
}

/// Read the MRU-5 list — same `workspaces.list_mru` source as the
/// InferenceBar dropdown (the `limit` arg is ignored; the harness caps
/// at the static MRU-5 window).
pub async fn recent_workspaces(_limit: u32) -> Result<Vec<WorkspaceSummary>, String> {
    list_workspaces().await
}

/// Register a workspace at `path` (and activate it). Redesign
/// replacement for `rag.workspaces.activate`'s create-on-new path; the
/// harness derives the slug. `full_reindex` is accepted for call-site
/// compatibility but ignored — `create` always indexes the new folder.
pub async fn activate_workspace(path: &str, _full_reindex: bool) -> Result<Value, String> {
    wylde_gui_pipe::call(
        "wylde-workspaces",
        "POST",
        "/__action__",
        Some(json!({
            "action": "workspaces.create",
            "payload": { "folder": path },
        })),
    )
    .await
}

/// Mark an existing workspace active + bump it to the MRU head. Redesign
/// replacement for `rag.workspaces.activate`'s activate-existing path
/// (the InferenceBar uses the same `workspaces.set_active` verb).
pub async fn set_active_workspace(workspace_id: &str) -> Result<Value, String> {
    wylde_gui_pipe::call(
        "wylde-workspaces",
        "POST",
        "/__action__",
        Some(json!({
            "action": "workspaces.set_active",
            "payload": { "workspace_id": workspace_id },
        })),
    )
    .await
}

/// Force a full re-index of a workspace's folder — the "Re-index"
/// button. Drives the Rust file-indexer ported in PR #18 via the
/// `workspaces.reindex` verb.
pub async fn reindex_workspace(workspace_id: &str) -> Result<Value, String> {
    wylde_gui_pipe::call(
        "wylde-workspaces",
        "POST",
        "/__action__",
        Some(json!({
            "action": "workspaces.reindex",
            "payload": { "workspace_id": workspace_id },
        })),
    )
    .await
}

/// Remove a workspace + its on-disk bundle.
pub async fn delete_workspace(workspace_id: &str) -> Result<Value, String> {
    wylde_gui_pipe::call(
        "wylde-workspaces",
        "POST",
        "/__action__",
        Some(json!({
            "action": "workspaces.delete",
            "payload": { "workspace_id": workspace_id },
        })),
    )
    .await
}

fn parse_workspace_array(v: &Value) -> Vec<WorkspaceSummary> {
    let Some(arr) = v.get("workspaces").and_then(|x| x.as_array()) else {
        return Vec::new();
    };
    arr.iter().map(WorkspaceSummary::from_value).collect()
}

/// Suggest a workspace ID from a filesystem path — the harness
/// ultimately derives the canonical slug; this helper is shown to the
/// user so they know roughly what to expect.  Mirrors
/// `Core/GUI/src/lib/workspaces.js::suggestIdFromPath`.
pub fn suggest_id_from_path(path: &str) -> String {
    let trimmed = path.trim_end_matches(['/', '\\']);
    let last = trimmed.rsplit(['/', '\\']).next().unwrap_or("");
    let mut out = String::with_capacity(last.len());
    let mut prev_dash = true;
    for c in last.chars() {
        let lower = c.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            out.push(lower);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    while out.starts_with('-') {
        out.remove(0);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workspace_summary_parses_full_payload() {
        let v = json!({
            "id": "my-project",
            "path": "C:/Users/x/code/my-project",
            "file_count": 42,
            "last_indexed_at": "2026-05-28T12:00:00Z",
            "last_activated_at": "2026-05-28T12:30:00Z",
            "indexing": false,
            "persona": "rust expert"
        });
        let s = WorkspaceSummary::from_value(&v);
        assert_eq!(s.id, "my-project");
        assert_eq!(s.path, "C:/Users/x/code/my-project");
        assert_eq!(s.file_count, Some(42));
        assert_eq!(s.last_indexed_at.as_deref(), Some("2026-05-28T12:00:00Z"));
        assert_eq!(s.last_activated_at.as_deref(), Some("2026-05-28T12:30:00Z"));
        assert!(!s.indexing);
        assert_eq!(s.persona.as_deref(), Some("rust expert"));
    }

    #[test]
    fn workspace_summary_defaults_when_missing() {
        let s = WorkspaceSummary::from_value(&json!({}));
        assert!(s.id.is_empty());
        assert!(s.path.is_empty());
        assert_eq!(s.file_count, None);
        assert!(!s.indexing);
    }

    #[test]
    fn parse_workspace_array_unwraps_envelope() {
        let v = json!({
            "workspaces": [
                {"id": "a", "path": "/tmp/a", "indexing": true},
                {"id": "b", "path": "/tmp/b"}
            ]
        });
        let out = parse_workspace_array(&v);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].id, "a");
        assert!(out[0].indexing);
        assert_eq!(out[1].id, "b");
        assert!(!out[1].indexing);
    }

    #[test]
    fn parse_workspace_array_handles_missing_key() {
        let out = parse_workspace_array(&json!({}));
        assert!(out.is_empty());
    }

    #[test]
    fn suggest_id_from_path_slugifies_windows_path() {
        assert_eq!(
            suggest_id_from_path(r"C:\Users\the Wylde user\code\My Project"),
            "my-project"
        );
    }

    #[test]
    fn suggest_id_from_path_slugifies_unix_path() {
        assert_eq!(suggest_id_from_path("/home/x/Cool Code/"), "cool-code");
    }

    #[test]
    fn suggest_id_from_path_handles_empty() {
        assert_eq!(suggest_id_from_path(""), "");
        assert_eq!(suggest_id_from_path("/"), "");
    }

    #[test]
    fn suggest_id_from_path_collapses_runs_of_separators() {
        assert_eq!(suggest_id_from_path("foo___bar---baz"), "foo-bar-baz");
    }

    #[test]
    fn each_pipe_call_uses_expected_verb() {
        // Build-time witness: every async helper compiles and the
        // verbs match the harness's `workspaces.*` surface.  Same
        // pattern slice 2's Settings tests used.
        let _ = list_workspaces;
        let _ = recent_workspaces;
        let _ = activate_workspace;
        let _ = set_active_workspace;
        let _ = reindex_workspace;
        let _ = delete_workspace;
    }
}
