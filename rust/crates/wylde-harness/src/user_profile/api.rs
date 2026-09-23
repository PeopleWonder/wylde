//! Action handlers for the `user_profile.*` verbs.
//!
//! Thought Bubble System Slice D. These are **in-process** harness verbs
//! (Build Order Appendix A: host `wylde-harness`, *in-process — no
//! pipe*). They're registered on the harness pipe like every other
//! harness verb, but they answer directly out of the local
//! [`store`](crate::user_profile::store) — there is no
//! `wylde-workspaces` round-trip and therefore no
//! `wylde-workspaces-client` timeout/retry/cache tier.
//!
//! > **Brief-vs-spec (same call the last three slices made — spec
//! > wins).** The slice brief tags `get`/`list_proposals` as
//! > "Fast · idempotent_read · 30s cache" and the writes as
//! > "Fast · NoRetry". Those are `wylde-workspaces-client` tiers, and
//! > they don't apply: Appendix A classes every `user_profile.*` verb as
//! > **in-process (no pipe), retry n/a, no cache**. A 30s read cache on a
//! > store the same process owns would only serve the user stale data
//! > right after their own edit. We follow Appendix A — no client tier,
//! > no cache — and note it here per precedent (Slices B / F-data /
//! > G-data).
//!
//! The JSON shaping lives here; [`crate::api::HarnessApi`] just
//! forwards to these functions.

use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};
use wylde_shared::ipc::{IpcError, Reply};

use crate::user_profile::profile::{apply_proposal, UserProfile};
use crate::user_profile::reflection::{self, ProposalCandidate};
use crate::user_profile::store;

/// `user_profile.get` — read the current profile. No payload.
pub async fn handle_get(_payload: Value) -> Reply {
    let profile = store::read().profile;
    Reply::ok(profile_value(&profile))
}

/// `user_profile.update` — apply a user edit (OI-18, user-edit-wins).
/// Payload is the patch object itself, or `{patch: {...}}`. Returns the
/// updated profile.
pub async fn handle_update(payload: Value) -> Reply {
    let patch = match extract_patch(&payload) {
        Some(p) => p,
        None => {
            return Reply::err(IpcError::new(
                "bad_request",
                "update requires a patch object (the fields to change, or {patch: {...}})",
            ))
        }
    };
    let result = store::with_store(|s| {
        s.profile.apply_patch(&patch);
        profile_value(&s.profile)
    });
    match result {
        Ok(v) => Reply::ok(v),
        Err(e) => Reply::err(IpcError::new(
            "io_error",
            format!("could not persist profile: {e}"),
        )),
    }
}

/// `user_profile.propose` — an LLM-proposed update enters the pending
/// queue, subject to the OI-7 spam-control gate. Payload:
/// `{field, proposed, confidence, current?, rationale?, conversation_id?}`.
///
/// A *refusal* by the spam-control gate is a normal outcome, returned as
/// `{accepted: false, reason, message}` with `ok = true`. Only a
/// malformed payload is an `ok = false` error.
pub async fn handle_propose(payload: Value) -> Reply {
    let cand = match parse_candidate(&payload) {
        Ok(c) => c,
        Err(e) => return Reply::err(e),
    };
    match reflection::propose(cand) {
        Ok(p) => Reply::ok(json!({
            "accepted": true,
            "proposal": serde_json::to_value(&p).unwrap_or(Value::Null),
        })),
        Err(reason) => Reply::ok(json!({
            "accepted": false,
            "reason": reason.code(),
            "message": reason.message(),
        })),
    }
}

/// `user_profile.accept` — apply a pending proposal to the profile and
/// drop it from the queue. Payload `{proposal_id}`. Returns the updated
/// profile.
pub async fn handle_accept(payload: Value) -> Reply {
    let id = match require_str(&payload, "proposal_id") {
        Ok(s) => s,
        Err(e) => return Reply::err(e),
    };
    let result = store::with_store(|s| {
        let pos = s.pending.iter().position(|p| p.id == id)?;
        let proposal = s.pending.remove(pos);
        apply_proposal(&mut s.profile, &proposal);
        Some(profile_value(&s.profile))
    });
    match result {
        Ok(Some(v)) => Reply::ok(v),
        Ok(None) => Reply::err(IpcError::new(
            "not_found",
            format!("no pending proposal {id}"),
        )),
        Err(e) => Reply::err(IpcError::new(
            "io_error",
            format!("could not persist profile: {e}"),
        )),
    }
}

/// `user_profile.reject` — drop a pending proposal and record it for the
/// OI-11 suppression window. Payload `{proposal_id}`. Returns
/// `{rejected: true, proposal_id}`.
pub async fn handle_reject(payload: Value) -> Reply {
    let id = match require_str(&payload, "proposal_id") {
        Ok(s) => s,
        Err(e) => return Reply::err(e),
    };
    let now = now_secs();
    let result = store::with_store(|s| {
        let Some(pos) = s.pending.iter().position(|p| p.id == id) else {
            return false;
        };
        let proposal = s.pending.remove(pos);
        reflection::record_rejection(&proposal.field, &proposal.proposed, now, s);
        true
    });
    match result {
        Ok(true) => Reply::ok(json!({ "rejected": true, "proposal_id": id })),
        Ok(false) => Reply::err(IpcError::new(
            "not_found",
            format!("no pending proposal {id}"),
        )),
        Err(e) => Reply::err(IpcError::new(
            "io_error",
            format!("could not persist profile: {e}"),
        )),
    }
}

/// `user_profile.list_proposals` — the pending queue (newest last). No
/// payload. Returns `{proposals: [...], count}`.
pub async fn handle_list_proposals(_payload: Value) -> Reply {
    let pending = store::read().pending;
    let count = pending.len();
    Reply::ok(json!({
        "proposals": serde_json::to_value(&pending).unwrap_or(Value::Array(vec![])),
        "count": count,
    }))
}

// ── helpers ───────────────────────────────────────────────────────────

fn profile_value(p: &UserProfile) -> Value {
    serde_json::to_value(p).unwrap_or(Value::Null)
}

/// Pull the patch object: prefer an explicit `{patch: {...}}`, else
/// treat the whole payload object as the patch (minus a stray `action`
/// key the dispatcher might leave on a direct call).
fn extract_patch(payload: &Value) -> Option<Map<String, Value>> {
    if let Some(obj) = payload.get("patch").and_then(Value::as_object) {
        return Some(obj.clone());
    }
    let obj = payload.as_object()?;
    if obj.is_empty() {
        return None;
    }
    let mut map = obj.clone();
    map.remove("action");
    Some(map)
}

fn parse_candidate(payload: &Value) -> Result<ProposalCandidate, IpcError> {
    let field = require_str(payload, "field")?;
    let proposed = require_str(payload, "proposed")?;
    let confidence = payload
        .get("confidence")
        .and_then(Value::as_f64)
        .ok_or_else(|| IpcError::new("bad_request", "confidence (number) is required"))?;
    Ok(ProposalCandidate {
        field,
        proposed,
        current: payload
            .get("current")
            .and_then(Value::as_str)
            .map(str::to_owned),
        rationale: payload
            .get("rationale")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned(),
        confidence,
        conversation_id: payload
            .get("conversation_id")
            .and_then(Value::as_str)
            .map(str::to_owned),
    })
}

fn require_str(payload: &Value, key: &str) -> Result<String, IpcError> {
    payload
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| {
            IpcError::new(
                "bad_request",
                format!("{key} (non-empty string) is required"),
            )
        })
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::user_profile::test_support::TestEnv;

    #[tokio::test]
    async fn get_returns_empty_profile_initially() {
        let _env = TestEnv::new();
        let reply = handle_get(Value::Null).await;
        assert!(reply.ok);
        assert_eq!(
            reply.data,
            serde_json::to_value(UserProfile::default()).unwrap()
        );
    }

    #[tokio::test]
    async fn update_applies_patch_and_persists() {
        let _env = TestEnv::new();
        let reply = handle_update(json!({"name": "Sam", "free_text_rules": "Be terse."})).await;
        assert!(reply.ok);
        assert_eq!(reply.data["name"], "Sam");
        assert_eq!(reply.data["free_text_rules"], "Be terse.");
        // Persisted.
        let again = handle_get(Value::Null).await;
        assert_eq!(again.data["name"], "Sam");
    }

    #[tokio::test]
    async fn update_accepts_wrapped_patch_form() {
        let _env = TestEnv::new();
        let reply = handle_update(json!({"patch": {"style": "dry"}})).await;
        assert!(reply.ok);
        assert_eq!(reply.data["style"], "dry");
    }

    #[tokio::test]
    async fn update_rejects_empty_payload() {
        let _env = TestEnv::new();
        let reply = handle_update(json!({})).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn propose_accept_lifecycle() {
        let _env = TestEnv::new();
        // Propose.
        let reply = handle_propose(json!({
            "field": "style", "proposed": "terse", "confidence": 0.9,
            "rationale": "you keep asking", "conversation_id": "c1"
        }))
        .await;
        assert!(reply.ok);
        assert_eq!(reply.data["accepted"], true);
        let pid = reply.data["proposal"]["id"].as_str().unwrap().to_owned();

        // It shows in list_proposals.
        let list = handle_list_proposals(Value::Null).await;
        assert_eq!(list.data["count"], 1);

        // Accept applies it to the profile and clears the queue.
        let acc = handle_accept(json!({"proposal_id": pid})).await;
        assert!(acc.ok);
        assert_eq!(acc.data["style"], "terse");
        let list = handle_list_proposals(Value::Null).await;
        assert_eq!(list.data["count"], 0);
    }

    #[tokio::test]
    async fn propose_refusal_is_ok_with_accepted_false() {
        let _env = TestEnv::new();
        let reply = handle_propose(json!({
            "field": "style", "proposed": "x", "confidence": 0.3
        }))
        .await;
        assert!(reply.ok, "a spam-gate refusal is a normal ok reply");
        assert_eq!(reply.data["accepted"], false);
        assert_eq!(reply.data["reason"], "low_confidence");
    }

    #[tokio::test]
    async fn propose_rejects_malformed_payload() {
        let _env = TestEnv::new();
        let reply = handle_propose(json!({"proposed": "x", "confidence": 0.9})).await;
        assert!(!reply.ok);
        assert_eq!(reply.error.unwrap().code, "bad_request");
    }

    #[tokio::test]
    async fn reject_records_suppression_and_blocks_repeat() {
        let _env = TestEnv::new();
        let reply = handle_propose(json!({
            "field": "style", "proposed": "terse", "confidence": 0.9
        }))
        .await;
        let pid = reply.data["proposal"]["id"].as_str().unwrap().to_owned();
        let rej = handle_reject(json!({"proposal_id": pid})).await;
        assert!(rej.ok);
        assert_eq!(rej.data["rejected"], true);

        // The same proposal is now suppressed (OI-11).
        let again = handle_propose(json!({
            "field": "style", "proposed": "terse", "confidence": 0.9
        }))
        .await;
        assert_eq!(again.data["accepted"], false);
        assert_eq!(again.data["reason"], "suppressed");
    }

    #[tokio::test]
    async fn accept_and_reject_unknown_id_are_not_found() {
        let _env = TestEnv::new();
        let acc = handle_accept(json!({"proposal_id": "nope"})).await;
        assert!(!acc.ok);
        assert_eq!(acc.error.unwrap().code, "not_found");
        let rej = handle_reject(json!({"proposal_id": "nope"})).await;
        assert_eq!(rej.error.unwrap().code, "not_found");
    }
}
