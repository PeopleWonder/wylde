//! The `workspaces.ignore.*` verb handlers (Slice M).
//!
//!   * `list` — both service-side tiers for a workspace (the conversation
//!     tier scoped to the given conversation id, empty when none given).
//!   * `add` / `remove` — mutate one tier. `add` is idempotent (re-adding
//!     reports `added: false` and succeeds — Appendix A classes it as an
//!     idempotent write); `remove` reports `removed: false` when absent.
//!
//! The global tier is the harness's (`chat/ignore/`); these verbs never see
//! it.

use serde_json::{json, Value};
use wylde_shared::ipc::{IpcError, Reply};

use super::store::{self, IgnoreEntry, IgnoreTier};

fn require_str(payload: &Value, key: &str) -> Option<String> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

fn entries_json(entries: &[IgnoreEntry]) -> Value {
    json!(entries
        .iter()
        .map(|e| json!({ "token": e.token, "added_at": e.added_at }))
        .collect::<Vec<_>>())
}

/// Parse `{tier, conversation_id?}` — `conversation` requires the id.
/// Returns the small [`IpcError`] (not a whole [`Reply`]) on a bad field so
/// the `Result` Err stays under the `result_large_err` threshold (the
/// anchors api's idiom); call sites wrap with [`Reply::err`].
fn parse_tier(payload: &Value) -> Result<(IgnoreTier, String), IpcError> {
    let Some(tier_s) = require_str(payload, "tier") else {
        return Err(IpcError::new(
            "bad_request",
            "tier is required (workspace | conversation)",
        ));
    };
    let Some(tier) = IgnoreTier::parse(&tier_s) else {
        return Err(IpcError::new(
            "bad_request",
            format!(
                "unknown tier '{tier_s}' (workspace | conversation; global lives in the harness)"
            ),
        ));
    };
    let conv = require_str(payload, "conversation_id").unwrap_or_default();
    if tier == IgnoreTier::Conversation && conv.is_empty() {
        return Err(IpcError::new(
            "bad_request",
            "conversation_id is required for the conversation tier",
        ));
    }
    Ok((tier, conv))
}

/// `workspaces.ignore.list` — payload `{workspace_id, conversation_id?}`.
pub async fn handle_list(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let conv = require_str(&payload, "conversation_id").unwrap_or_default();
    let file = store::load(&ws);
    Reply::ok(json!({
        "workspace_id": ws,
        "workspace": entries_json(&file.workspace),
        "conversation": entries_json(file.conversation(&conv)),
        "conversation_id": conv,
    }))
}

/// `workspaces.ignore.add` — payload
/// `{workspace_id, tier, token, conversation_id?}`.
pub async fn handle_add(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(token) = require_str(&payload, "token") else {
        return Reply::err_msg("bad_request", "token is required");
    };
    let (tier, conv) = match parse_tier(&payload) {
        Ok(t) => t,
        Err(e) => return Reply::err(e),
    };
    match store::add(&ws, tier, &conv, &token) {
        Ok(added) => Reply::ok(json!({
            "ok": true,
            "added": added,
            "workspace_id": ws,
            "token": token,
        })),
        Err(e) => Reply::err_msg("io_error", format!("write ignore.json: {e}")),
    }
}

/// `workspaces.ignore.remove` — payload
/// `{workspace_id, tier, token, conversation_id?}`.
pub async fn handle_remove(payload: Value) -> Reply {
    let Some(ws) = require_str(&payload, "workspace_id") else {
        return Reply::err_msg("bad_request", "workspace_id is required");
    };
    let Some(token) = require_str(&payload, "token") else {
        return Reply::err_msg("bad_request", "token is required");
    };
    let (tier, conv) = match parse_tier(&payload) {
        Ok(t) => t,
        Err(e) => return Reply::err(e),
    };
    match store::remove(&ws, tier, &conv, &token) {
        Ok(removed) => Reply::ok(json!({
            "ok": true,
            "removed": removed,
            "workspace_id": ws,
            "token": token,
        })),
        Err(e) => Reply::err_msg("io_error", format!("write ignore.json: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestEnv;

    fn ok_value(r: Reply) -> Value {
        assert!(r.ok, "expected ok, got {:?}", r.error);
        r.data
    }

    fn is_bad_request(r: &Reply) -> bool {
        !r.ok && r.error.as_ref().is_some_and(|e| e.code == "bad_request")
    }

    #[tokio::test]
    async fn add_list_remove_over_the_verb_surface() {
        let _env = TestEnv::new();
        let ws = "ws-ignore-verbs";

        let add = ok_value(
            handle_add(json!({
                "workspace_id": ws, "tier": "workspace", "token": "noisy_helper"
            }))
            .await,
        );
        assert_eq!(add["added"], true);

        // Idempotent re-add: still ok, added=false.
        let again = ok_value(
            handle_add(json!({
                "workspace_id": ws, "tier": "workspace", "token": "noisy_helper"
            }))
            .await,
        );
        assert_eq!(again["ok"], true);
        assert_eq!(again["added"], false);

        ok_value(
            handle_add(json!({
                "workspace_id": ws, "tier": "conversation",
                "conversation_id": "conv-9", "token": "scratch_fn"
            }))
            .await,
        );

        let list =
            ok_value(handle_list(json!({ "workspace_id": ws, "conversation_id": "conv-9" })).await);
        assert_eq!(list["workspace"].as_array().unwrap().len(), 1);
        assert_eq!(list["conversation"].as_array().unwrap().len(), 1);
        assert_eq!(list["conversation"][0]["token"], "scratch_fn");

        // A different conversation sees no conversation-tier entries.
        let other =
            ok_value(handle_list(json!({ "workspace_id": ws, "conversation_id": "conv-x" })).await);
        assert!(other["conversation"].as_array().unwrap().is_empty());

        let rm = ok_value(
            handle_remove(json!({
                "workspace_id": ws, "tier": "workspace", "token": "noisy_helper"
            }))
            .await,
        );
        assert_eq!(rm["removed"], true);
        let rm2 = ok_value(
            handle_remove(json!({
                "workspace_id": ws, "tier": "workspace", "token": "noisy_helper"
            }))
            .await,
        );
        assert_eq!(rm2["removed"], false);
    }

    #[tokio::test]
    async fn bad_requests_carry_the_exact_contract() {
        let _env = TestEnv::new();
        assert!(is_bad_request(
            &handle_add(json!({ "tier": "workspace", "token": "x" })).await
        ));
        assert!(is_bad_request(
            &handle_add(json!({ "workspace_id": "w", "tier": "workspace" })).await
        ));
        assert!(is_bad_request(
            &handle_add(json!({ "workspace_id": "w", "tier": "global", "token": "x" })).await
        ));
        assert!(is_bad_request(
            &handle_add(json!({ "workspace_id": "w", "tier": "conversation", "token": "x" })).await
        ));
        assert!(is_bad_request(&handle_list(json!({})).await));
    }
}
