//! IPC + model for the Files tab (IDE S5).
//!
//! Backed by the jailed `workspaces.fs.list_dir` verb (S1) — lazy
//! per-directory expansion (OQ-4). The active workspace id comes from the same
//! `workspaces.list_mru` source the InferenceBar dropdown uses.

use serde_json::{json, Value};

/// Kind of a directory entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
}

/// One entry in a listed directory, with its workspace-relative path resolved.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub name: String,
    pub kind: Kind,
    /// Workspace-relative path (parent joined with name, `/`-separated).
    pub rel_path: String,
    /// True when the indexer's walk would skip this (`.git`, `target`,
    /// dotfiles, binary suffixes). Still listed, but dimmed (OQ-7).
    pub ignored: bool,
}

/// The active workspace id (or `None` if there is no active workspace).
pub async fn active_workspace_id() -> Result<Option<String>, String> {
    let v = wylde_gui_pipe::call(
        "wylde-workspaces",
        "POST",
        "/__action__",
        Some(json!({ "action": "workspaces.list_mru", "payload": {} })),
    )
    .await?;
    Ok(v.get("active_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned))
}

/// List one directory level under `workspace_id` at workspace-relative
/// `parent` (`""` = root). Returns entries with resolved relative paths,
/// dirs-first (the service already sorts).
pub async fn list_dir(workspace_id: &str, parent: &str) -> Result<Vec<Entry>, String> {
    let v = wylde_gui_pipe::call(
        "wylde-workspaces",
        "POST",
        "/__action__",
        Some(json!({
            "action": "workspaces.fs.list_dir",
            "payload": { "workspace_id": workspace_id, "path": parent },
        })),
    )
    .await?;
    Ok(parse_entries(&v, parent))
}

/// Join a workspace-relative `parent` with a child `name` using `/`.
pub fn join_rel(parent: &str, name: &str) -> String {
    if parent.is_empty() {
        name.to_owned()
    } else {
        format!("{}/{}", parent.trim_end_matches('/'), name)
    }
}

fn parse_entries(v: &Value, parent: &str) -> Vec<Entry> {
    let Some(arr) = v.get("entries").and_then(Value::as_array) else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|e| {
            let name = e.get("name").and_then(Value::as_str)?.to_owned();
            let kind = match e.get("kind").and_then(Value::as_str) {
                Some("dir") => Kind::Dir,
                Some("symlink") => Kind::Symlink,
                _ => Kind::File,
            };
            let ignored = e.get("ignored").and_then(Value::as_bool).unwrap_or(false);
            let rel_path = join_rel(parent, &name);
            Some(Entry {
                name,
                kind,
                rel_path,
                ignored,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn join_rel_roots_and_nests() {
        assert_eq!(join_rel("", "src"), "src");
        assert_eq!(join_rel("src", "main.rs"), "src/main.rs");
        assert_eq!(join_rel("a/b", "c"), "a/b/c");
    }

    #[test]
    fn parse_entries_resolves_paths_and_kinds() {
        let v = json!({
            "entries": [
                { "name": "src", "kind": "dir", "ignored": false },
                { "name": "main.rs", "kind": "file", "ignored": false },
                { "name": "target", "kind": "dir", "ignored": true },
                { "name": "link", "kind": "symlink" },
            ]
        });
        let out = parse_entries(&v, "");
        assert_eq!(out.len(), 4);
        assert_eq!(out[0].kind, Kind::Dir);
        assert_eq!(out[0].rel_path, "src");
        assert_eq!(out[1].rel_path, "main.rs");
        assert!(out[2].ignored);
        assert_eq!(out[3].kind, Kind::Symlink);
    }

    #[test]
    fn parse_entries_nested_parent() {
        let v = json!({ "entries": [{ "name": "deep.rs", "kind": "file" }] });
        let out = parse_entries(&v, "src/inner");
        assert_eq!(out[0].rel_path, "src/inner/deep.rs");
    }

    #[test]
    fn missing_entries_is_empty() {
        assert!(parse_entries(&json!({}), "").is_empty());
    }
}
