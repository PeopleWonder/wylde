//! The `workspaces.anchors.*` verb handlers (Slice N-data).
//!
//! Eight verbs over the per-workspace anchor [`store`]:
//!
//!   * `list` / `create` / `update` / `delete` — CRUD.
//!   * `find_by_token` — resolve `{{token}}` → anchors (composer recognition).
//!   * `find_by_target` — inverse lookup `symbol_id` → anchors (OI-20).
//!   * `list_under` — hierarchy traversal by `parent_anchor` (OI-19).
//!   * `propose` — the LLM reflection candidate (user-accept-always, OI-7/18).
//!
//! Every reply embeds the same [`Anchor`] wire shape ([`Anchor::to_value`]) the
//! harness global store returns, so the two scopes are byte-identical.

use serde_json::{json, Value};
use wylde_shared::anchor::{already_exists_global_details, AnchorKind, AnchorTarget};
use wylde_shared::ipc::{IpcError, Reply};

use super::anchor::{workspace_anchor, Anchor};
use super::reflection::{self, ReflectionBudget};
use super::store::{self, AnchorPatch, CreateOutcome};
use super::tokenizer::{is_valid_identifier, normalize_token};

fn require_str(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// Parse `kind` + `target` from a create/propose payload. Defaults `kind` to
/// `concept` when omitted (the common case for free-text anchors); `target`
/// is required and must be a well-formed tagged value.
///
/// Returns the small [`IpcError`] (not a whole [`Reply`]) on a bad field so the
/// `Result` Err stays under the `result_large_err` threshold; the call site
/// wraps it with [`Reply::err`].
fn parse_kind_target(payload: &Value) -> Result<(AnchorKind, AnchorTarget), IpcError> {
    let kind = match payload.get("kind") {
        None | Some(Value::Null) => AnchorKind::Concept,
        Some(v) => serde_json::from_value(v.clone())
            .map_err(|e| IpcError::new("bad_request", format!("invalid kind: {e}")))?,
    };
    let target_val = payload
        .get("target")
        .filter(|v| !v.is_null())
        .ok_or_else(|| IpcError::new("bad_request", "target is required"))?;
    let target: AnchorTarget = serde_json::from_value(target_val.clone())
        .map_err(|e| IpcError::new("bad_request", format!("invalid target: {e}")))?;
    Ok((kind, target))
}

/// Apply optional `parent_anchor` / `domain` / `related_to` fields from a
/// payload onto a freshly built anchor.
fn apply_optional_fields(anchor: &mut Anchor, payload: &Value) {
    if let Some(p) = payload.get("parent_anchor").and_then(Value::as_str) {
        let p = p.trim();
        if !p.is_empty() {
            anchor.parent_anchor = Some(p.to_owned());
        }
    }
    if let Some(d) = payload.get("domain").and_then(Value::as_str) {
        let d = d.trim();
        if !d.is_empty() {
            anchor.domain = Some(d.to_owned());
        }
    }
    if let Some(arr) = payload.get("related_to").and_then(Value::as_array) {
        anchor.related_to = arr
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
    }
}

/// Build the create/propose anchor from a payload, validating the identifier.
fn build_anchor(workspace_id: &str, payload: &Value) -> Result<Anchor, IpcError> {
    let Some(identifier) = require_str(payload, "identifier") else {
        return Err(IpcError::new("bad_request", "identifier is required"));
    };
    if !is_valid_identifier(&identifier) {
        return Err(IpcError::new(
            "bad_request",
            "identifier must be alphanumeric + underscore (no spaces/punctuation)",
        ));
    }
    let (kind, target) = parse_kind_target(payload)?;
    let description = payload
        .get("description")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let mut anchor = workspace_anchor(workspace_id, identifier, kind, target, description);
    apply_optional_fields(&mut anchor, payload);
    Ok(anchor)
}

fn anchors_reply(workspace_id: &str, extra: &[(&str, Value)], anchors: Vec<Anchor>) -> Reply {
    let mut obj = json!({
        "workspace_id": workspace_id,
        "count": anchors.len(),
        "anchors": anchors.iter().map(Anchor::to_value).collect::<Vec<_>>(),
    });
    for (k, v) in extra {
        obj[*k] = v.clone();
    }
    Reply::ok(obj)
}

/// `workspaces.anchors.list` — every anchor for a workspace.
pub async fn handle_list(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    anchors_reply(&ws, &[], store::load(&ws))
}

/// `workspaces.anchors.create` — mint a workspace anchor. A duplicate
/// identifier in this workspace returns `already_exists` (not a second
/// record), carrying the existing definition in `details`.
pub async fn handle_create(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let anchor = match build_anchor(&ws, &payload) {
        Ok(a) => a,
        Err(e) => return Reply::err(e),
    };
    match store::create(&ws, anchor) {
        Ok(CreateOutcome::Created(a)) => Reply::ok(a.to_value()),
        Ok(CreateOutcome::AlreadyExists(existing)) => Reply::err(IpcError {
            code: "already_exists".into(),
            message: format!(
                "anchor {:?} already exists in this workspace",
                existing.identifier
            ),
            details: Some(already_exists_global_details(&existing)),
        }),
        Err(e) => Reply::err_msg("io_error", format!("write anchors.json: {e}")),
    }
}

/// `workspaces.anchors.update` — patch an existing anchor's
/// description/target/related_to/parent_anchor/domain. `not_found` for an
/// unknown identifier.
pub async fn handle_update(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(identifier) = require_str(&payload, "identifier") else {
        return Reply::err_msg("bad_request", "identifier is required");
    };

    let target = match payload.get("target") {
        Some(v) if !v.is_null() => match serde_json::from_value::<AnchorTarget>(v.clone()) {
            Ok(t) => Some(t),
            Err(e) => return Reply::err_msg("bad_request", format!("invalid target: {e}")),
        },
        _ => None,
    };
    let patch = AnchorPatch {
        description: payload
            .get("description")
            .and_then(Value::as_str)
            .map(str::to_owned),
        target,
        related_to: payload
            .get("related_to")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            }),
        // `parent_anchor: null` in the payload clears the parent; absent leaves
        // it. An empty string also clears.
        parent_anchor: payload.get("parent_anchor").map(|v| {
            v.as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        }),
        domain: payload.get("domain").map(|v| {
            v.as_str()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        }),
    };

    match store::update(&ws, &identifier, patch) {
        Ok(Some(a)) => Reply::ok(a.to_value()),
        Ok(None) => Reply::err_msg("not_found", format!("anchor {identifier:?} not found")),
        Err(e) => Reply::err_msg("io_error", format!("write anchors.json: {e}")),
    }
}

/// `workspaces.anchors.delete` — remove an anchor by identifier.
pub async fn handle_delete(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(identifier) = require_str(&payload, "identifier") else {
        return Reply::err_msg("bad_request", "identifier is required");
    };
    match store::delete(&ws, &identifier) {
        Ok(ok) => Reply::ok(json!({ "ok": ok, "identifier": identifier })),
        Err(e) => Reply::err_msg("io_error", format!("write anchors.json: {e}")),
    }
}

/// `workspaces.anchors.find_by_token` — resolve `{{token}}` → anchors. Accepts
/// either `{{name}}` or bare `name`.
pub async fn handle_find_by_token(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(raw) = require_str(&payload, "token") else {
        return Reply::err_msg("bad_request", "token is required");
    };
    let Some(token) = normalize_token(&raw) else {
        return Reply::err_msg("bad_request", "token is not a valid identifier");
    };
    let anchors = store::find_by_token(&ws, &token);
    anchors_reply(&ws, &[("token", json!(token))], anchors)
}

/// `workspaces.anchors.find_by_target` — inverse lookup `symbol_id` → anchors
/// (OI-20).
pub async fn handle_find_by_target(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(symbol_id) = require_str(&payload, "symbol_id") else {
        return Reply::err_msg("bad_request", "symbol_id is required");
    };
    let anchors = store::find_by_target(&ws, &symbol_id);
    anchors_reply(&ws, &[("symbol_id", json!(symbol_id))], anchors)
}

/// `workspaces.anchors.list_under` — anchors under a taxonomy parent (OI-19).
pub async fn handle_list_under(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(parent_id) = require_str(&payload, "parent_id") else {
        return Reply::err_msg("bad_request", "parent_id is required");
    };
    let anchors = store::list_under(&ws, &parent_id);
    anchors_reply(&ws, &[("parent_id", json!(parent_id))], anchors)
}

/// `workspaces.anchors.propose` — an LLM reflection candidate (NOT persisted;
/// the user accepts it via `create`). Applies the OI-7 spam-control gate using
/// counters the caller supplies (`confidence`, `proposals_so_far`,
/// `last_proposal_at`). Reply: `{candidate}` or `{candidate: null, reason}`.
pub async fn handle_propose(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let anchor = match build_anchor(&ws, &payload) {
        Ok(a) => a,
        Err(e) => return Reply::err(e),
    };
    let confidence = payload
        .get("confidence")
        .and_then(Value::as_f64)
        .unwrap_or(1.0) as f32;
    let rationale = payload
        .get("rationale")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let budget = ReflectionBudget {
        proposals_so_far: payload
            .get("proposals_so_far")
            .and_then(Value::as_u64)
            .unwrap_or(0) as u32,
        last_proposal_at: payload.get("last_proposal_at").and_then(Value::as_f64),
    };
    let now = super::anchor::epoch_now();

    match reflection::propose(anchor, confidence, rationale, budget, now) {
        Ok(p) => Reply::ok(json!({
            "candidate": p.anchor.to_value(),
            "confidence": p.confidence,
            "rationale": p.rationale,
        })),
        Err(reason) => Reply::ok(json!({
            "candidate": Value::Null,
            "reason": reason.as_str(),
        })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestEnv;

    fn create_payload(ws: &str, id: &str) -> Value {
        json!({
            "workspace_id": ws,
            "identifier": id,
            "kind": "concept",
            "target": { "type": "concept", "text": format!("def {id}") },
            "description": format!("desc {id}"),
        })
    }

    #[tokio::test]
    async fn create_list_find_delete_lifecycle() {
        let _env = TestEnv::new();
        let ws = "ws-api-000000";

        let created = handle_create(create_payload(ws, "alpha")).await;
        assert!(created.ok, "{:?}", created.error);
        assert_eq!(created.data["identifier"], "alpha");
        assert_eq!(created.data["scope"]["scope"], "workspace");

        let listed = handle_list(json!({ "workspace_id": ws })).await;
        assert_eq!(listed.data["count"], 1);

        let found = handle_find_by_token(json!({ "workspace_id": ws, "token": "{{alpha}}" })).await;
        assert!(found.ok);
        assert_eq!(found.data["count"], 1);
        assert_eq!(found.data["token"], "alpha");

        let del = handle_delete(json!({ "workspace_id": ws, "identifier": "alpha" })).await;
        assert_eq!(del.data["ok"], true);
        assert_eq!(
            handle_list(json!({ "workspace_id": ws })).await.data["count"],
            0
        );
    }

    #[tokio::test]
    async fn create_rejects_invalid_identifier() {
        let _env = TestEnv::new();
        let r = handle_create(json!({
            "workspace_id": "ws",
            "identifier": "bad name",
            "target": { "type": "concept", "text": "t" },
        }))
        .await;
        assert!(!r.ok);
        assert_eq!(r.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn create_requires_target() {
        let _env = TestEnv::new();
        let r = handle_create(json!({ "workspace_id": "ws", "identifier": "ok" })).await;
        assert!(!r.ok);
        assert!(r.error.unwrap().message.contains("target"));
    }

    #[tokio::test]
    async fn duplicate_create_returns_already_exists_with_details() {
        let _env = TestEnv::new();
        let ws = "ws-dup-api-000000";
        handle_create(create_payload(ws, "same")).await;
        let dup = handle_create(create_payload(ws, "same")).await;
        assert!(!dup.ok);
        let err = dup.error.unwrap();
        assert_eq!(err.code, "already_exists");
        let details = err.details.expect("details present");
        assert_eq!(details["identifier"], "same");
        assert_eq!(details["existing_definition"], "desc same");
    }

    #[tokio::test]
    async fn update_patches_then_not_found() {
        let _env = TestEnv::new();
        let ws = "ws-upd-api-000000";
        handle_create(create_payload(ws, "edit")).await;
        let upd = handle_update(json!({
            "workspace_id": ws, "identifier": "edit",
            "description": "patched", "domain": "Networking",
        }))
        .await;
        assert!(upd.ok);
        assert_eq!(upd.data["description"], "patched");
        assert_eq!(upd.data["domain"], "Networking");

        let miss = handle_update(json!({ "workspace_id": ws, "identifier": "ghost" })).await;
        assert_eq!(miss.error.unwrap().code, "not_found");
    }

    #[tokio::test]
    async fn find_by_target_and_list_under() {
        let _env = TestEnv::new();
        let ws = "ws-find-api-000000";
        // A code-symbol anchor + a child under a parent.
        handle_create(json!({
            "workspace_id": ws, "identifier": "the_fn", "kind": "code_symbol",
            "target": { "type": "code_symbol", "symbol_id": "run_it" },
            "description": "runs",
        }))
        .await;
        handle_create(create_payload(ws, "parent_topic")).await;
        handle_create(json!({
            "workspace_id": ws, "identifier": "child_topic", "kind": "concept",
            "target": { "type": "concept", "text": "t" },
            "description": "d", "parent_anchor": "parent_topic",
        }))
        .await;

        let by_target =
            handle_find_by_target(json!({ "workspace_id": ws, "symbol_id": "run_it" })).await;
        assert_eq!(by_target.data["count"], 1);
        assert_eq!(by_target.data["anchors"][0]["identifier"], "the_fn");

        let under =
            handle_list_under(json!({ "workspace_id": ws, "parent_id": "parent_topic" })).await;
        assert_eq!(under.data["count"], 1);
        assert_eq!(under.data["anchors"][0]["identifier"], "child_topic");
    }

    #[tokio::test]
    async fn propose_gates_on_confidence() {
        let _env = TestEnv::new();
        let ws = "ws-prop-000000";
        let low = handle_propose(json!({
            "workspace_id": ws, "identifier": "maybe",
            "target": { "type": "concept", "text": "t" },
            "confidence": 0.5,
        }))
        .await;
        assert!(low.ok);
        assert!(low.data["candidate"].is_null());
        assert_eq!(low.data["reason"], "low_confidence");

        let ok = handle_propose(json!({
            "workspace_id": ws, "identifier": "yes_anchor",
            "target": { "type": "concept", "text": "t" },
            "confidence": 0.9, "rationale": "recurred",
        }))
        .await;
        assert!(ok.ok);
        assert_eq!(ok.data["candidate"]["identifier"], "yes_anchor");
        // Proposal is NOT persisted.
        assert_eq!(
            handle_list(json!({ "workspace_id": ws })).await.data["count"],
            0
        );
    }

    #[tokio::test]
    async fn list_requires_workspace_id() {
        let _env = TestEnv::new();
        let r = handle_list(json!({})).await;
        assert_eq!(r.error.unwrap().code, "bad_request");
    }
}
