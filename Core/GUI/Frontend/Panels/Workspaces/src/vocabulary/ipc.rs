//! Vocabulary tab → pipe calls (Slice N): the workspace anchor store
//! (`workspaces.anchors.*` on `wylde-workspaces`) and the global store
//! (`anchors.*` in-process on the harness). OI-1 graceful degrade — an
//! unreachable side surfaces as a banner, never an empty-looking tab lying
//! about the data.
//!
//! `AnchorView` mirrors the shared `Anchor` wire shape locally (the same
//! decoupling convention as `graph/model/` — the GUI crate doesn't link the
//! service crates; serde defaults keep older records loading).

use serde::Deserialize;
use serde_json::{json, Value};

const SVC_WORKSPACES: &str = "wylde-workspaces";
const SVC_HARNESS: &str = "wylde-harness";

/// The GUI mirror of one anchor record (wylde-shared `Anchor` wire shape).
#[derive(Clone, Debug, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct AnchorView {
    pub identifier: String,
    /// `symbol | concept | convention | person` (serde of `AnchorKind`).
    pub kind: Value,
    /// Internally-tagged target: `{type: code_symbol, symbol_id}` or
    /// `{type: concept, text}`.
    pub target: Value,
    pub description: String,
    pub aliases: Vec<String>,
    pub related_to: Vec<String>,
    pub parent_anchor: Option<String>,
    pub domain: Option<String>,
    pub created_at: f64,
    pub last_used_at: f64,
    pub usage_count: u32,
}

impl AnchorView {
    /// `code_symbol` target's id, when this anchors a symbol.
    pub fn target_symbol(&self) -> Option<&str> {
        if self.target.get("type").and_then(Value::as_str) == Some("code_symbol") {
            self.target.get("symbol_id").and_then(Value::as_str)
        } else {
            None
        }
    }

    /// The concept text, when this anchors a free-text definition.
    pub fn target_text(&self) -> Option<&str> {
        if self.target.get("type").and_then(Value::as_str) == Some("concept") {
            self.target.get("text").and_then(Value::as_str)
        } else {
            None
        }
    }

    pub fn kind_label(&self) -> String {
        self.kind.as_str().unwrap_or("concept").replace('_', " ")
    }
}

/// Which store a row lives in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AnchorScopeTag {
    Workspace,
    Global,
}

async fn workspaces_call(action: &str, payload: Value) -> Result<Value, String> {
    wylde_gui_pipe::call(
        SVC_WORKSPACES,
        "POST",
        "/__action__",
        Some(json!({ "action": action, "payload": payload })),
    )
    .await
}

async fn harness_call(action: &str, payload: Value) -> Result<Value, String> {
    wylde_gui_pipe::call(
        SVC_HARNESS,
        "POST",
        "/__action__",
        Some(json!({ "action": action, "payload": payload })),
    )
    .await
}

fn parse_anchors(v: &Value) -> Vec<AnchorView> {
    v.get("anchors")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(|a| serde_json::from_value::<AnchorView>(a.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The active workspace id (`workspaces.list_mru` → `active_id`), `None`
/// when no workspace is active.
pub async fn active_workspace() -> Result<Option<String>, String> {
    let v = workspaces_call("workspaces.list_mru", json!({})).await?;
    Ok(v.get("active_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned))
}

/// Every workspace-scope anchor for `ws`.
pub async fn list_workspace_anchors(ws: &str) -> Result<Vec<AnchorView>, String> {
    let v = workspaces_call("workspaces.anchors.list", json!({ "workspace_id": ws })).await?;
    Ok(parse_anchors(&v))
}

/// Every global anchor.
pub async fn list_global_anchors() -> Result<Vec<AnchorView>, String> {
    let v = harness_call("anchors.list", json!({})).await?;
    Ok(parse_anchors(&v))
}

/// Create a concept anchor (the Vocabulary tab's "New anchor" — symbol
/// targets are created from the composer/graph flows, not typed by hand).
pub async fn create_workspace_anchor(
    ws: &str,
    identifier: &str,
    description: &str,
    definition: &str,
) -> Result<Value, String> {
    workspaces_call(
        "workspaces.anchors.create",
        json!({
            "workspace_id": ws,
            "identifier": identifier,
            "kind": "concept",
            "target": { "type": "concept", "text": definition },
            "description": description,
        }),
    )
    .await
}

/// Patch an anchor's editable fields (either store, keyed by scope).
pub async fn update_anchor(
    scope: AnchorScopeTag,
    ws: &str,
    identifier: &str,
    description: &str,
    aliases: &[String],
    domain: Option<&str>,
    parent: Option<&str>,
) -> Result<Value, String> {
    let mut payload = json!({
        "identifier": identifier,
        "description": description,
        "aliases": aliases,
        "domain": domain,
        "parent_anchor": parent,
    });
    match scope {
        AnchorScopeTag::Workspace => {
            payload["workspace_id"] = json!(ws);
            workspaces_call("workspaces.anchors.update", payload).await
        }
        AnchorScopeTag::Global => harness_call("anchors.update", payload).await,
    }
}

/// Delete an anchor from its store.
pub async fn delete_anchor(
    scope: AnchorScopeTag,
    ws: &str,
    identifier: &str,
) -> Result<Value, String> {
    match scope {
        AnchorScopeTag::Workspace => {
            workspaces_call(
                "workspaces.anchors.delete",
                json!({ "workspace_id": ws, "identifier": identifier }),
            )
            .await
        }
        AnchorScopeTag::Global => {
            harness_call("anchors.delete", json!({ "identifier": identifier })).await
        }
    }
}

/// Land a workspace anchor in the GLOBAL store (the promotion landing point,
/// Plan §4.4: promotion = `anchors.create` with the whole record). The OI-5
/// collision comes back as the `already_exists_global` error code with the
/// existing definition in its details.
pub async fn promote_to_global(
    anchor: &AnchorView,
    rename_to: Option<&str>,
) -> Result<Value, String> {
    let identifier = rename_to.unwrap_or(&anchor.identifier);
    harness_call(
        "anchors.create",
        json!({
            "identifier": identifier,
            "kind": anchor.kind,
            "target": anchor.target,
            "description": anchor.description,
            "aliases": anchor.aliases,
            "related_to": anchor.related_to,
            "parent_anchor": anchor.parent_anchor,
            "domain": anchor.domain,
        }),
    )
    .await
}

/// Replace the existing global definition (the OI-5 "Replace" choice —
/// explicit-confirm only; `anchors.update` patches the existing record
/// wholesale with this anchor's fields).
pub async fn replace_global(anchor: &AnchorView) -> Result<Value, String> {
    harness_call(
        "anchors.update",
        json!({
            "identifier": anchor.identifier,
            "description": anchor.description,
            "target": anchor.target,
            "aliases": anchor.aliases,
            "related_to": anchor.related_to,
            "parent_anchor": anchor.parent_anchor,
            "domain": anchor.domain,
        }),
    )
    .await
}

/// Is `err` (a stringified pipe error) the OI-5 global-collision signal?
pub fn is_global_collision(err: &str) -> bool {
    err.contains("already_exists_global")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anchor_view_parses_the_wire_shape() {
        let v = json!({
            "identifier": "the_pipe_protocol",
            "kind": "concept",
            "target": { "type": "concept", "text": "msgpack over named pipes" },
            "scope": { "scope": "workspace", "workspace_id": "ws-1" },
            "description": "How services talk",
            "aliases": ["pipe protocol"],
            "related_to": ["wire_format"],
            "parent_anchor": "ipc",
            "domain": "Networking",
            "created_at": 100.0,
            "last_used_at": 200.0,
            "usage_count": 7
        });
        let a: AnchorView = serde_json::from_value(v).unwrap();
        assert_eq!(a.identifier, "the_pipe_protocol");
        assert_eq!(a.kind_label(), "concept");
        assert_eq!(a.target_text(), Some("msgpack over named pipes"));
        assert_eq!(a.target_symbol(), None);
        assert_eq!(a.aliases, vec!["pipe protocol"]);
        assert_eq!(a.domain.as_deref(), Some("Networking"));
        assert_eq!(a.usage_count, 7);
    }

    #[test]
    fn symbol_target_resolves_and_old_records_default() {
        let a: AnchorView = serde_json::from_value(json!({
            "identifier": "set_active",
            "target": { "type": "code_symbol", "symbol_id": "set_active" }
        }))
        .unwrap();
        assert_eq!(a.target_symbol(), Some("set_active"));
        assert_eq!(a.target_text(), None);
        assert!(a.aliases.is_empty(), "serde defaults fill missing fields");
        assert_eq!(a.usage_count, 0);
    }

    #[test]
    fn collision_detection_matches_the_oi5_code() {
        assert!(is_global_collision(
            "already_exists_global: {{x}} exists with definition…"
        ));
        assert!(!is_global_collision("transport: pipe closed"));
    }
}
