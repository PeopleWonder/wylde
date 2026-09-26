//! Resources — `resources/list` + `resources/read` over the `wylde://` namespace.
//!
//! Conversations resolve through the harness `conversations.*` actions. The
//! workspace registry (`workspaces.list_mru`) lives on the `wylde-workspaces`
//! service: the harness retired `workspaces.*` in Slice 0d and answers
//! `no_action`. Workspace files are read straight off disk under the workspace
//! root, confined to that root, size-capped (H2) and read with `tokio::fs` (H1).

use serde_json::{json, Value};

use super::adapters::{call, entries, BridgeError, URI_SCHEME};
use crate::services::ollama::{harness_service, workspaces_service};

/// The pipe services resources resolve against. Production resolves both from
/// env (with the shipped defaults); tests pass unique mock pipe names.
struct Services {
    harness: String,
    workspaces: String,
}

impl Services {
    fn resolve() -> Self {
        Self {
            harness: harness_service(),
            workspaces: workspaces_service(),
        }
    }
}

/// Enumerate readable resources: recent conversations + workspaces.
///
/// Conversations are listed first, then workspaces. Each source is
/// best-effort: one unreachable service drops only its own entries, and the
/// call fails only when neither answers.
pub async fn list_resources() -> Result<Value, BridgeError> {
    list_resources_via(&Services::resolve()).await
}

async fn list_resources_via(svc: &Services) -> Result<Value, BridgeError> {
    let mut out: Vec<Value> = Vec::new();
    let convs = call(&svc.harness, "conversations.list", json!({})).await;
    let wss = call(&svc.workspaces, "workspaces.list_mru", json!({})).await;
    let (convs, wss) = match (convs, wss) {
        (Err(e), Err(_)) => return Err(e),
        (c, w) => {
            for (what, r) in [("conversations", &c), ("workspaces", &w)] {
                if let Err(e) = r {
                    tracing::warn!(error = %e.message, "mcp resources/list: {what} unavailable");
                }
            }
            (c.unwrap_or(Value::Null), w.unwrap_or(Value::Null))
        }
    };
    for conv in entries(&convs, "conversations") {
        let cid = conv.get("id").and_then(Value::as_str).unwrap_or("");
        if cid.is_empty() {
            continue;
        }
        let name = conv
            .get("title")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(cid);
        out.push(json!({
            "uri": format!("{URI_SCHEME}conversation/{cid}"),
            "name": name,
            "mimeType": "application/json",
        }));
    }
    for ws in entries(&wss, "workspaces") {
        let wid = ws.get("id").and_then(Value::as_str).unwrap_or("");
        if wid.is_empty() {
            continue;
        }
        let name = ws
            .get("folder")
            .or_else(|| ws.get("path"))
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .unwrap_or(wid);
        out.push(json!({
            "uri": format!("{URI_SCHEME}workspace/{wid}/"),
            "name": name,
            "mimeType": "inode/directory",
        }));
    }
    Ok(Value::Array(out))
}

/// A parsed `wylde://` resource URI.
#[derive(Debug, PartialEq, Eq)]
pub enum ResourceRef {
    Conversation(String),
    Workspace { id: String, path: String },
}

/// Split a `wylde://` URI into a [`ResourceRef`].
///
/// * `wylde://conversation/<id>`             → `Conversation(id)`
/// * `wylde://workspace/<workspace_id>/<p>`  → `Workspace { id, path }`
///
/// Returns `None` for any other shape.
pub fn parse_uri(uri: &str) -> Option<ResourceRef> {
    let rest = uri.strip_prefix(URI_SCHEME)?;
    let mut parts = rest.splitn(3, '/');
    match parts.next()? {
        "conversation" => {
            let id = parts.next().filter(|s| !s.is_empty())?;
            Some(ResourceRef::Conversation(id.to_owned()))
        }
        "workspace" => {
            let id = parts.next().filter(|s| !s.is_empty())?;
            let path = parts.next().unwrap_or("");
            Some(ResourceRef::Workspace {
                id: id.to_owned(),
                path: path.to_owned(),
            })
        }
        _ => None,
    }
}

/// Resolve a `wylde://` URI to its MCP `contents` block.
pub async fn read_resource(uri: &str) -> Result<Value, BridgeError> {
    read_resource_via(&Services::resolve(), uri).await
}

async fn read_resource_via(svc: &Services, uri: &str) -> Result<Value, BridgeError> {
    match parse_uri(uri) {
        None => Err(BridgeError::msg(format!(
            "unsupported resource uri: {uri:?}"
        ))),
        Some(ResourceRef::Conversation(id)) => {
            let doc = call(&svc.harness, "conversations.get", json!({ "id": id })).await?;
            let text = serde_json::to_string(&doc).unwrap_or_else(|_| "null".to_owned());
            Ok(json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "application/json",
                    "text": text,
                }]
            }))
        }
        Some(ResourceRef::Workspace { id, path }) => {
            if path.is_empty() {
                return Err(BridgeError::msg(format!(
                    "workspace resource uri needs a file path: {uri:?}"
                )));
            }
            let text = read_workspace_file(&svc.workspaces, &id, &path).await?;
            Ok(json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "text/plain",
                    "text": text,
                }]
            }))
        }
    }
}

/// Read a file under a workspace's folder. The workspace root comes from
/// the `workspaces.list_mru` registry on the workspaces service.
async fn read_workspace_file(
    workspaces: &str,
    workspace_id: &str,
    rel_path: &str,
) -> Result<String, BridgeError> {
    let wss = call(workspaces, "workspaces.list_mru", json!({})).await?;
    let workspace = entries(&wss, "workspaces")
        .into_iter()
        .find(|w| w.get("id").and_then(Value::as_str) == Some(workspace_id))
        .ok_or_else(|| BridgeError::msg(format!("workspace not found: {workspace_id:?}")))?;
    let root = workspace
        .get("folder")
        .or_else(|| workspace.get("path"))
        .and_then(Value::as_str)
        .unwrap_or("");
    if root.is_empty() {
        return Err(BridgeError::msg(format!(
            "workspace {workspace_id:?} has no indexed path"
        )));
    }
    resolve_and_read(root, rel_path).await
}

/// Largest workspace file `resources/read` will return over MCP. A read
/// is refused above this before any bytes are loaded, so a huge (or
/// runaway) file cannot spike gateway memory (H2). 1 MiB comfortably
/// covers source and text resources.
pub const MAX_RESOURCE_READ_BYTES: u64 = 1_048_576;

/// Resolve `rel_path` against `root`, confine it to the workspace, and
/// read it as UTF-8 text. A `../` that escapes the workspace root is
/// rejected; a file over [`MAX_RESOURCE_READ_BYTES`] or one that is not
/// valid UTF-8 is rejected rather than read.
///
/// All filesystem calls are `tokio::fs` so a resource read never blocks a
/// runtime worker thread (H1).
pub async fn resolve_and_read(root: &str, rel_path: &str) -> Result<String, BridgeError> {
    let base = tokio::fs::canonicalize(root)
        .await
        .map_err(|exc| BridgeError::msg(format!("workspace root unavailable: {exc}")))?;
    let target = tokio::fs::canonicalize(base.join(rel_path))
        .await
        .map_err(|_| BridgeError::msg(format!("file not found in workspace: {rel_path:?}")))?;
    if !target.starts_with(&base) {
        return Err(BridgeError::msg(format!(
            "path escapes workspace root: {rel_path:?}"
        )));
    }
    let meta = tokio::fs::metadata(&target)
        .await
        .map_err(|exc| BridgeError::msg(format!("could not stat workspace file: {exc}")))?;
    if !meta.is_file() {
        return Err(BridgeError::msg(format!(
            "file not found in workspace: {rel_path:?}"
        )));
    }
    // Cap BEFORE reading so an oversized file never enters memory.
    if meta.len() > MAX_RESOURCE_READ_BYTES {
        return Err(BridgeError::msg(format!(
            "workspace file too large for MCP read: {rel_path:?} is {} bytes (cap {MAX_RESOURCE_READ_BYTES})",
            meta.len()
        )));
    }
    let bytes = tokio::fs::read(&target)
        .await
        .map_err(|exc| BridgeError::msg(format!("could not read workspace file: {exc}")))?;
    // MCP text content must be UTF-8 — refuse binary rather than emit
    // lossy/garbled text.
    String::from_utf8(bytes)
        .map_err(|_| BridgeError::msg(format!("workspace file is not UTF-8 text: {rel_path:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_uri_handles_conversation() {
        assert_eq!(
            parse_uri("wylde://conversation/abc-123"),
            Some(ResourceRef::Conversation("abc-123".to_owned()))
        );
    }

    #[test]
    fn parse_uri_handles_workspace_with_nested_path() {
        assert_eq!(
            parse_uri("wylde://workspace/ws-9/src/main.rs"),
            Some(ResourceRef::Workspace {
                id: "ws-9".to_owned(),
                path: "src/main.rs".to_owned(),
            })
        );
    }

    #[test]
    fn parse_uri_workspace_root_has_empty_path() {
        assert_eq!(
            parse_uri("wylde://workspace/ws-9/"),
            Some(ResourceRef::Workspace {
                id: "ws-9".to_owned(),
                path: String::new(),
            })
        );
    }

    #[test]
    fn parse_uri_rejects_foreign_scheme_and_unknown_kind() {
        assert_eq!(parse_uri("https://example.test/x"), None);
        assert_eq!(parse_uri("wylde://memory/abc"), None);
        assert_eq!(parse_uri("wylde://conversation/"), None);
        assert_eq!(parse_uri("wylde://"), None);
    }

    /// A mock `wylde-workspaces` pipe (unique name, never the production one)
    /// serving `workspaces.list_mru` with one workspace rooted at a tempdir
    /// holding `note.txt`. The harness name points at a pipe nobody serves, so
    /// any workspace data that comes back can only have come from the
    /// workspaces service (#357: the harness answers `no_action`).
    async fn mock_services() -> &'static (Services, tempfile::TempDir) {
        use tokio::sync::OnceCell;
        use wylde_shared::ipc;
        static M: OnceCell<(Services, tempfile::TempDir)> = OnceCell::const_new();
        M.get_or_init(|| async {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("note.txt"), "from the workspace").unwrap();
            let folder = dir.path().to_string_lossy().into_owned();
            ipc::register_action("workspaces.list_mru", move |_p: Value| {
                let folder = folder.clone();
                async move {
                    ipc::Reply::ok(json!({"workspaces": [{"id": "ws-mock", "folder": folder}]}))
                }
            });
            let id = uuid::Uuid::new_v4().simple();
            let workspaces = format!("mcp-res-ws-{id}");
            let server = std::sync::Arc::new(ipc::PipeServer::new(&workspaces));
            std::thread::spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("server runtime");
                let _ = rt.block_on(server.accept_loop()); // wylde-check: discard-result-ok
            });
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let svc = Services {
                harness: format!("mcp-res-no-harness-{id}"),
                workspaces,
            };
            (svc, dir)
        })
        .await
    }

    #[tokio::test]
    async fn list_resolves_workspaces_from_the_workspaces_service() {
        let (svc, _) = mock_services().await;
        // The harness is unreachable: conversations drop out, the call still
        // succeeds with the workspace from the workspaces service.
        let list = list_resources_via(svc).await.expect("workspaces answered");
        let uris: Vec<&str> = list
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|r| r["uri"].as_str())
            .collect();
        assert_eq!(uris, ["wylde://workspace/ws-mock/"]);
    }

    #[tokio::test]
    async fn read_resolves_the_workspace_root_from_the_workspaces_service() {
        let (svc, _) = mock_services().await;
        let got = read_resource_via(svc, "wylde://workspace/ws-mock/note.txt")
            .await
            .expect("read through the workspaces service");
        assert_eq!(got["contents"][0]["text"], "from the workspace");
    }

    #[tokio::test]
    async fn list_fails_only_when_both_services_are_unreachable() {
        let id = uuid::Uuid::new_v4().simple();
        let svc = Services {
            harness: format!("mcp-res-none-h-{id}"),
            workspaces: format!("mcp-res-none-w-{id}"),
        };
        assert!(list_resources_via(&svc).await.is_err());
    }

    #[tokio::test]
    async fn resolve_and_read_reads_a_file_inside_the_workspace() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("note.txt");
        std::fs::write(&file, "hello workspace").unwrap();
        let root = dir.path().to_str().unwrap();
        assert_eq!(
            resolve_and_read(root, "note.txt").await.unwrap(),
            "hello workspace"
        );
    }

    #[tokio::test]
    async fn resolve_and_read_rejects_traversal_outside_the_workspace() {
        let outer = tempfile::tempdir().unwrap();
        std::fs::write(outer.path().join("secret.txt"), "top secret").unwrap();
        let inner = outer.path().join("workspace");
        std::fs::create_dir(&inner).unwrap();
        let root = inner.to_str().unwrap();
        // `../secret.txt` resolves outside the workspace root.
        let err = resolve_and_read(root, "../secret.txt").await.unwrap_err();
        assert!(
            err.message.contains("escapes") || err.message.contains("not found"),
            "unexpected message: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn resolve_and_read_rejects_a_file_over_the_size_cap() {
        let dir = tempfile::tempdir().unwrap();
        let big = vec![b'a'; (MAX_RESOURCE_READ_BYTES + 1) as usize];
        std::fs::write(dir.path().join("big.txt"), &big).unwrap();
        let err = resolve_and_read(dir.path().to_str().unwrap(), "big.txt")
            .await
            .unwrap_err();
        assert!(
            err.message.contains("too large"),
            "unexpected message: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn resolve_and_read_rejects_non_utf8_binary() {
        let dir = tempfile::tempdir().unwrap();
        // Invalid UTF-8 byte sequence.
        std::fs::write(dir.path().join("bin.dat"), [0xff, 0xfe, 0x00, 0x9f]).unwrap();
        let err = resolve_and_read(dir.path().to_str().unwrap(), "bin.dat")
            .await
            .unwrap_err();
        assert!(
            err.message.contains("not UTF-8"),
            "unexpected message: {}",
            err.message
        );
    }
}
