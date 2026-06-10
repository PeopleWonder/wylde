//! The global `anchors.*` verb handlers (Slice N-data, harness half).
//!
//! In-process verbs (no pipe round-trip — the harness owns global state):
//!
//!   * `anchors.list` / `create` / `update` / `delete` — CRUD (Build Order §3).
//!   * `anchors.find_by_token` / `find_by_target` / `list_under` — the same
//!     read surface the workspace store exposes, so a consumer (the composer /
//!     chat turn driver) can resolve `{{tokens}}`, do the inverse lookup
//!     (OI-20), and traverse the hierarchy (OI-19) across **both** scopes with
//!     symmetric calls. (Build Order §3 names the four CRUD verbs; the three
//!     reads mirror the workspace side per the slice brief — additive, and
//!     needed by the Phase-4 composer's cross-store resolution.)
//!
//! Replies embed the identical [`Anchor`] wire shape ([`Anchor::to_value`]) the
//! `workspaces.anchors.*` verbs return — the two scopes are byte-identical.
//!
//! ## Collision policy (OI-5)
//!
//! `anchors.create` is the promotion landing point. A duplicate identifier
//! returns the structured `already_exists_global` error — its `details` carry
//! the existing definition so the GUI Vocabulary tab can render the rename /
//! keep-workspace-only / replace dialog. The data layer never overwrites.

use serde_json::{json, Value};
use wylde_shared::anchor::{
    already_exists_global_details, Anchor, AnchorKind, AnchorScope, AnchorTarget,
};
use wylde_shared::anchor_tokenizer::{is_valid_identifier, normalize_lookup_token};
use wylde_shared::ipc::{IpcError, Reply};

use super::store::{self, AnchorPatch, CreateOutcome, UpdateOutcome};

fn require_str(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

// Helpers return the small `IpcError` (not a whole `Reply`) so the `Result`
// Err stays under the `result_large_err` threshold; call sites wrap it with
// `Reply::err`.
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
    if let Some(aliases) = parse_aliases(payload) {
        anchor.aliases = aliases;
    }
}

/// Extract the raw `aliases` string array from a payload (un-normalised — the
/// store's `validate_aliases` normalises + collision-checks). `None` if absent.
fn parse_aliases(payload: &Value) -> Option<Vec<String>> {
    payload.get("aliases").and_then(Value::as_array).map(|arr| {
        arr.iter()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect()
    })
}

/// Build a global-scoped anchor from a create payload, validating the
/// identifier.
fn build_global_anchor(payload: &Value) -> Result<Anchor, IpcError> {
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
    let mut anchor = Anchor::new(identifier, kind, target, AnchorScope::Global, description);
    apply_optional_fields(&mut anchor, payload);
    Ok(anchor)
}

fn anchors_reply(extra: &[(&str, Value)], anchors: Vec<Anchor>) -> Reply {
    let mut obj = json!({
        "scope": "global",
        "count": anchors.len(),
        "anchors": anchors.iter().map(Anchor::to_value).collect::<Vec<_>>(),
    });
    for (k, v) in extra {
        obj[*k] = v.clone();
    }
    Reply::ok(obj)
}

/// `anchors.list` — every global anchor.
pub async fn handle_list(_payload: Value) -> Reply {
    anchors_reply(&[], store::load())
}

/// `anchors.create` — promote/mint a global anchor. A duplicate identifier
/// returns `already_exists_global` with the existing definition in `details`
/// (OI-5 collision); never an overwrite.
pub async fn handle_create(payload: Value) -> Reply {
    let anchor = match build_global_anchor(&payload) {
        Ok(a) => a,
        Err(e) => return Reply::err(e),
    };
    match store::create(anchor) {
        Ok(CreateOutcome::Created(a)) => Reply::ok(a.to_value()),
        Ok(CreateOutcome::AlreadyExists(existing)) => Reply::err(IpcError {
            code: "already_exists_global".into(),
            message: format!(
                "token {:?} already exists globally (as an identifier or alias)",
                existing.identifier
            ),
            details: Some(already_exists_global_details(&existing)),
        }),
        Ok(CreateOutcome::AliasRejected(e)) => Reply::err(e.into_ipc()),
        Err(e) => Reply::err_msg("io_error", format!("write global_anchors.json: {e}")),
    }
}

/// `anchors.update` — patch a global anchor. not_found for an unknown
/// identifier.
pub async fn handle_update(payload: Value) -> Reply {
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
        aliases: parse_aliases(&payload),
        related_to: payload
            .get("related_to")
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            }),
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
        // Archive / unarchive (OI-21 Recommended Cleanup, Slice N).
        archived: payload.get("archived").and_then(Value::as_bool),
    };
    match store::update(&identifier, patch) {
        Ok(UpdateOutcome::Updated(a)) => Reply::ok(a.to_value()),
        Ok(UpdateOutcome::NotFound) => Reply::err_msg(
            "not_found",
            format!("global anchor {identifier:?} not found"),
        ),
        Ok(UpdateOutcome::AliasRejected(e)) => Reply::err(e.into_ipc()),
        Err(e) => Reply::err_msg("io_error", format!("write global_anchors.json: {e}")),
    }
}

/// `anchors.delete` — remove a global anchor by identifier.
pub async fn handle_delete(payload: Value) -> Reply {
    let Some(identifier) = require_str(&payload, "identifier") else {
        return Reply::err_msg("bad_request", "identifier is required");
    };
    match store::delete(&identifier) {
        Ok(ok) => Reply::ok(json!({ "ok": ok, "identifier": identifier })),
        Err(e) => Reply::err_msg("io_error", format!("write global_anchors.json: {e}")),
    }
}

/// `anchors.find_by_token` — resolve `{{token}}` → global anchors, matching the
/// canonical `identifier` or any human-friendly `alias` (spaces permitted, so a
/// multi-word alias resolves). Accepts `{{name}}` or bare `name`.
pub async fn handle_find_by_token(payload: Value) -> Reply {
    let Some(raw) = require_str(&payload, "token") else {
        return Reply::err_msg("bad_request", "token is required");
    };
    let Some(token) = normalize_lookup_token(&raw) else {
        return Reply::err_msg("bad_request", "token is empty");
    };
    anchors_reply(&[("token", json!(token))], store::find_by_token(&token))
}

/// `anchors.promote_via_alias` — the **global promotion landing point** for an
/// alias-driven promotion (Slice N-data-aliases). Behaves identically to
/// [`handle_create`] (mints a global anchor with the full payload — **all
/// aliases preserved** — and the same OI-5 `already_exists_global` collision
/// policy), but the verb name documents that the user promoted via an alias and
/// it audit-logs that intent (the optional `via_alias` field). This is where
/// the whole anchor lands globally; the workspace-side
/// `workspaces.anchors.promote_via_alias` hands the promotion payload here.
pub async fn handle_promote_via_alias(payload: Value) -> Reply {
    let via_alias = payload
        .get("via_alias")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_owned();
    let anchor = match build_global_anchor(&payload) {
        Ok(a) => a,
        Err(e) => return Reply::err(e),
    };
    tracing::info!(
        anchor = %anchor.identifier,
        via_alias = %via_alias,
        aliases = anchor.aliases.len(),
        "anchors.promote_via_alias: landing whole-anchor promotion in global store"
    );
    match store::create(anchor) {
        Ok(CreateOutcome::Created(a)) => Reply::ok(a.to_value()),
        Ok(CreateOutcome::AlreadyExists(existing)) => Reply::err(IpcError {
            code: "already_exists_global".into(),
            message: format!(
                "token {:?} already exists globally (as an identifier or alias)",
                existing.identifier
            ),
            details: Some(already_exists_global_details(&existing)),
        }),
        Ok(CreateOutcome::AliasRejected(e)) => Reply::err(e.into_ipc()),
        Err(e) => Reply::err_msg("io_error", format!("write global_anchors.json: {e}")),
    }
}

/// `anchors.find_by_target` — inverse lookup `symbol_id` → global anchors
/// (OI-20).
pub async fn handle_find_by_target(payload: Value) -> Reply {
    let Some(symbol_id) = require_str(&payload, "symbol_id") else {
        return Reply::err_msg("bad_request", "symbol_id is required");
    };
    anchors_reply(
        &[("symbol_id", json!(symbol_id))],
        store::find_by_target(&symbol_id),
    )
}

/// `anchors.list_under` — global anchors under a taxonomy parent (OI-19).
pub async fn handle_list_under(payload: Value) -> Reply {
    let Some(parent_id) = require_str(&payload, "parent_id") else {
        return Reply::err_msg("bad_request", "parent_id is required");
    };
    anchors_reply(
        &[("parent_id", json!(parent_id))],
        store::list_under(&parent_id),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::MutexGuard;
    use tempfile::TempDir;

    struct Env {
        _g: MutexGuard<'static, ()>,
        _td: TempDir,
        prior: Option<std::ffi::OsString>,
    }
    impl Env {
        fn new() -> Self {
            let g = crate::memory::common::TEST_ENV_LOCK
                .lock()
                .unwrap_or_else(|p| p.into_inner());
            let td = TempDir::new().unwrap();
            let prior = std::env::var_os("WYLDE_DATA_DIR");
            std::env::set_var("WYLDE_DATA_DIR", td.path());
            Self {
                _g: g,
                _td: td,
                prior,
            }
        }
    }
    impl Drop for Env {
        fn drop(&mut self) {
            match self.prior.take() {
                Some(v) => std::env::set_var("WYLDE_DATA_DIR", v),
                None => std::env::remove_var("WYLDE_DATA_DIR"),
            }
        }
    }

    fn create_payload(id: &str) -> Value {
        json!({
            "identifier": id,
            "kind": "concept",
            "target": { "type": "concept", "text": format!("def {id}") },
            "description": format!("desc {id}"),
        })
    }

    #[tokio::test]
    async fn create_list_delete_lifecycle() {
        let _e = Env::new();
        let created = handle_create(create_payload("g1")).await;
        assert!(created.ok, "{:?}", created.error);
        assert_eq!(created.data["scope"]["scope"], "global");

        let listed = handle_list(Value::Null).await;
        assert_eq!(listed.data["count"], 1);
        assert_eq!(listed.data["scope"], "global");

        let del = handle_delete(json!({ "identifier": "g1" })).await;
        assert_eq!(del.data["ok"], true);
    }

    #[tokio::test]
    async fn collision_returns_already_exists_global_with_details() {
        let _e = Env::new();
        handle_create(create_payload("the_pipe_protocol")).await;
        let dup = handle_create(create_payload("the_pipe_protocol")).await;
        assert!(!dup.ok);
        let err = dup.error.unwrap();
        assert_eq!(err.code, "already_exists_global");
        let details = err.details.expect("details");
        assert_eq!(details["identifier"], "the_pipe_protocol");
        assert_eq!(details["existing_definition"], "desc the_pipe_protocol");
        // The existing record is surfaced for the rename/keep/replace dialog.
        assert_eq!(details["existing"]["scope"]["scope"], "global");
    }

    #[tokio::test]
    async fn find_by_token_normalizes_braces() {
        let _e = Env::new();
        handle_create(create_payload("wanted")).await;
        let found = handle_find_by_token(json!({ "token": "{{wanted}}" })).await;
        assert!(found.ok);
        assert_eq!(found.data["count"], 1);
        assert_eq!(found.data["token"], "wanted");
    }

    #[tokio::test]
    async fn update_not_found_then_found() {
        let _e = Env::new();
        let miss = handle_update(json!({ "identifier": "ghost", "description": "x" })).await;
        assert_eq!(miss.error.unwrap().code, "not_found");
        handle_create(create_payload("real")).await;
        let ok = handle_update(json!({ "identifier": "real", "description": "patched" })).await;
        assert_eq!(ok.data["description"], "patched");
    }

    #[tokio::test]
    async fn create_requires_target() {
        let _e = Env::new();
        let r = handle_create(json!({ "identifier": "x" })).await;
        assert!(!r.ok);
        assert!(r.error.unwrap().message.contains("target"));
    }

    // ── Slice N-data-aliases ────────────────────────────────────────────

    #[tokio::test]
    async fn promote_via_alias_lands_whole_anchor_with_all_aliases() {
        let _e = Env::new();
        // Simulate the workspace-side payload landing in the global store.
        let promoted = handle_promote_via_alias(json!({
            "identifier": "set_active_graph_view", "kind": "concept",
            "target": { "type": "concept", "text": "switch view" },
            "description": "the view switch",
            "aliases": ["set active", "graph view"],
            "via_alias": "set active",
        }))
        .await;
        assert!(promoted.ok, "{:?}", promoted.error);
        assert_eq!(promoted.data["scope"]["scope"], "global");
        // All aliases preserved through promotion.
        assert_eq!(promoted.data["aliases"][0], "set active");
        assert_eq!(promoted.data["aliases"][1], "graph view");

        // It is now globally resolvable via its alias (canonical returned).
        let found = handle_find_by_token(json!({ "token": "{{graph view}}" })).await;
        assert!(found.ok);
        assert_eq!(found.data["count"], 1);
        assert_eq!(
            found.data["anchors"][0]["identifier"],
            "set_active_graph_view"
        );
    }

    #[tokio::test]
    async fn promote_via_alias_collision_is_already_exists_global() {
        let _e = Env::new();
        handle_create(create_payload("taken")).await;
        let dup = handle_promote_via_alias(json!({
            "identifier": "taken",
            "target": { "type": "concept", "text": "t" },
        }))
        .await;
        assert!(!dup.ok);
        assert_eq!(dup.error.unwrap().code, "already_exists_global");
    }
}
