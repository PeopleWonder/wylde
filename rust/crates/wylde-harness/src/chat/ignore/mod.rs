//! Global symbol ignore list (Slice M, Plan v2 §5.8) — the harness tier.
//!
//! "Ignore" = *default to inactive from now on*: an ignored token still
//! highlights in the composer, but rides along deselected unless reactivated
//! for one message (↺). The workspace + conversation tiers live in
//! `wylde-workspaces` (`workspaces.ignore.*`); this is the cross-workspace
//! tier, stored beside the global anchor store it mirrors ([`store`]).
//!
//! Three in-process verbs — `ignore.{list,add,remove}` — registered on the
//! harness pipe (global-anchors precedent: user-level, not
//! workspace-scoped). The turn driver consults [`store::is_ignored`] (plus
//! the service tiers over the client) during context gather.

pub mod store;

use serde_json::{json, Value};
use wylde_shared::ipc::Reply;

fn require_token(payload: &Value) -> Option<String> {
    payload
        .get("token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// `ignore.list` — every globally ignored token. No payload.
pub async fn handle_list(_payload: Value) -> Reply {
    let entries = store::load();
    Reply::ok(json!({
        "scope": "global",
        "ignored": entries
            .iter()
            .map(|e| json!({ "token": e.token, "added_at": e.added_at }))
            .collect::<Vec<_>>(),
        "count": entries.len(),
    }))
}

/// `ignore.add` — ignore a token globally. Payload: `{token}`. Idempotent
/// (re-adds succeed with `added: false`).
pub async fn handle_add(payload: Value) -> Reply {
    let Some(token) = require_token(&payload) else {
        return Reply::err_msg("bad_request", "token is required");
    };
    match store::add(&token) {
        Ok(added) => Reply::ok(json!({ "ok": true, "added": added, "token": token })),
        Err(e) => Reply::err_msg("io_error", format!("write global_ignore.json: {e}")),
    }
}

/// `ignore.remove` — stop ignoring a token globally. Payload: `{token}`.
pub async fn handle_remove(payload: Value) -> Reply {
    let Some(token) = require_token(&payload) else {
        return Reply::err_msg("bad_request", "token is required");
    };
    match store::remove(&token) {
        Ok(removed) => Reply::ok(json!({ "ok": true, "removed": removed, "token": token })),
        Err(e) => Reply::err_msg("io_error", format!("write global_ignore.json: {e}")),
    }
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

    #[tokio::test]
    async fn verb_surface_round_trips() {
        let _env = Env::new();
        let add = handle_add(json!({ "token": "loud_logger" })).await;
        assert!(add.ok);
        assert_eq!(add.data["added"], true);

        let again = handle_add(json!({ "token": "loud_logger" })).await;
        assert!(again.ok, "idempotent");
        assert_eq!(again.data["added"], false);

        let list = handle_list(json!({})).await;
        assert!(list.ok);
        assert_eq!(list.data["count"], 1);
        assert_eq!(list.data["ignored"][0]["token"], "loud_logger");

        let rm = handle_remove(json!({ "token": "loud_logger" })).await;
        assert!(rm.ok);
        assert_eq!(rm.data["removed"], true);
        assert_eq!(handle_list(json!({})).await.data["count"], 0);
    }

    #[tokio::test]
    async fn missing_token_is_bad_request() {
        let _env = Env::new();
        for r in [
            handle_add(json!({})).await,
            handle_remove(json!({ "token": "  " })).await,
        ] {
            assert!(!r.ok);
            assert_eq!(r.error.as_ref().unwrap().code, "bad_request");
        }
    }
}
