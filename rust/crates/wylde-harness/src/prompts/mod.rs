//! `prompts.*` — system-prompt overrides and presets (5 verbs).
//!
//! Rust port of `Core/harness/pipe/_prompts.py` +
//! `Core/shared/system_prompts{,_catalog}.py` (full-Rust cutover,
//! 2026-06-09). The Settings page's prompt-editor section reads and
//! mutates the override store through these handlers; every reply is
//! the same one-round-trip envelope the Python actions returned:
//!
//! ```json
//! {
//!   "groups":        [...],   // catalog groups
//!   "catalog":       [...],   // catalog entries (id/group/label/desc/default)
//!   "overrides":     {...},   // current override map
//!   "presets":       {...},   // saved preset bundles
//!   "active_preset": "Default"
//! }
//! ```
//!
//! * [`catalog`] — embedded defaults/groups (`catalog.json`, verbatim
//!   from the retired Python catalog).
//! * [`store`] — the `data/system_prompts.json` bundle (same file +
//!   format as Python; no migration needed).
//!
//! Other subsystems read effective prompt text through
//! [`store::effective_prompt`] — override if set, catalog default
//! otherwise — replacing the Python `system_prompts.effective_prompt`.

pub mod catalog;
pub mod store;

use serde_json::{json, Value};
use wylde_shared::ipc::Reply;

use store::{Store, StoreError};

/// The `prompts.*` reply envelope (groups + catalog + the store bundle).
fn envelope(s: &Store) -> Value {
    let bundle = s.to_json();
    json!({
        "groups": catalog::groups_json(),
        "catalog": catalog::catalog_json(),
        "overrides": bundle["overrides"],
        "presets": bundle["presets"],
        "active_preset": bundle["active_preset"],
    })
}

fn reply_from(result: Result<Store, StoreError>) -> Reply {
    match result {
        Ok(s) => Reply::ok(envelope(&s)),
        Err(StoreError::BadRequest(m)) => Reply::err_msg("bad_request", m),
        Err(StoreError::NotFound(m)) => Reply::err_msg("not_found", m),
        Err(StoreError::Io(m)) => Reply::err_msg("io_error", m),
    }
}

/// Require a non-empty string field, mirroring the Python `_ActionError`
/// guards (`"<field> is required"`).
fn require_str(payload: &Value, field: &str) -> Option<String> {
    payload
        .get(field)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

/// `prompts.list {}` — groups + catalog + overrides + presets +
/// active_preset. The Settings page hits this on mount.
pub fn handle_list(_payload: Value) -> Reply {
    Reply::ok(envelope(&store::read_store()))
}

/// `prompts.save {id, text?}` — save an override for one prompt id.
/// `text: null` (or text matching the catalog default) clears it.
pub fn handle_save(payload: Value) -> Reply {
    let Some(id) = require_str(&payload, "id") else {
        return Reply::err_msg("bad_request", "id is required");
    };
    let text = match payload.get("text") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.as_str()),
        Some(_) => return Reply::err_msg("bad_request", "text must be a string or null"),
    };
    reply_from(store::set_override(&id, text))
}

/// `prompts.save_preset {name}` — snapshot the current overrides into a
/// named preset and activate it.
pub fn handle_save_preset(payload: Value) -> Reply {
    let Some(name) = require_str(&payload, "name") else {
        return Reply::err_msg("bad_request", "name is required");
    };
    reply_from(store::save_preset(&name))
}

/// `prompts.set_active {name}` — activate the named preset (`"Default"`
/// resets to catalog defaults).
pub fn handle_set_active(payload: Value) -> Reply {
    let Some(name) = require_str(&payload, "name") else {
        return Reply::err_msg("bad_request", "name is required");
    };
    reply_from(store::load_preset(&name))
}

/// `prompts.delete_preset {name}` — remove a named preset; active falls
/// back to `Default` if it was the one deleted.
pub fn handle_delete_preset(payload: Value) -> Reply {
    let Some(name) = require_str(&payload, "name") else {
        return Reply::err_msg("bad_request", "name is required");
    };
    reply_from(store::delete_preset(&name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_carries_catalog_and_store_fields() {
        let v = envelope(&Store::default());
        for key in ["groups", "catalog", "overrides", "presets", "active_preset"] {
            assert!(v.get(key).is_some(), "envelope missing {key}");
        }
        assert!(!v["catalog"].as_array().unwrap().is_empty());
    }

    #[test]
    fn save_rejects_missing_id_and_bad_text() {
        assert!(!handle_save(json!({})).ok);
        assert!(!handle_save(json!({"id": "inference_bar.chat", "text": 7})).ok);
    }

    #[test]
    fn preset_verbs_reject_missing_name() {
        assert!(!handle_save_preset(json!({})).ok);
        assert!(!handle_set_active(json!({})).ok);
        assert!(!handle_delete_preset(json!({})).ok);
    }
}
