//! Consent-verb machinery for the `consent.*` surface (Phase 12.2 +
//! 12.6) -- snapshot shaping, the set/respond decision path, and the
//! pending-prompt stream loop, plus their unit tests. Split from
//! `api.rs` per architecture-review R1; the `HarnessApi` methods in
//! `api/mod.rs` delegate here.

use serde_json::{json, Value};
use wylde_shared::ipc::{Reply, StreamSender};

use crate::tooling::consent::{self, Decision};

use super::helpers::require_string;

pub(super) fn consent_snapshot_value() -> Value {
    let snap = consent::store().snapshot();
    let tools = snap
        .tools
        .iter()
        .map(|(k, v)| (k.clone(), Value::String(v.as_wire().to_string())))
        .collect::<serde_json::Map<_, _>>();
    json!({
        "no_auth": snap.no_auth,
        "tools": tools,
    })
}

fn parse_consent_set(payload: &Value) -> Result<(String, Decision, bool), String> {
    let Some(tool_id) = require_string(payload, "tool_id") else {
        return Err("tool_id is required".to_owned());
    };
    let decision_str = payload
        .get("decision")
        .and_then(Value::as_str)
        .ok_or_else(|| "decision is required".to_owned())?;
    let decision = match decision_str {
        "approved" => Decision::Approved,
        "denied" => Decision::Denied,
        other => {
            return Err(format!(
                "decision must be \"approved\" or \"denied\"; got {other:?}"
            ))
        }
    };
    // Phase 12.6: `remember: false` makes the decision authorize the
    // current call without writing to disk. Missing → default true to
    // preserve pre-12.6 behaviour for callers that don't know about
    // the flag. A non-bool value is a bad_request — we don't want a
    // string "false" to silently mean "persist".
    let remember = match payload.get("remember") {
        None => true,
        Some(Value::Bool(b)) => *b,
        Some(_) => return Err("remember must be a bool".to_owned()),
    };
    Ok((tool_id, decision, remember))
}

pub(super) fn handle_consent_decide(payload: &Value) -> Reply {
    match parse_consent_set(payload) {
        Ok((tool_id, decision, remember)) => {
            let resolved_decision = match decision {
                Decision::Approved => "approved",
                Decision::Denied => "denied",
            };
            if remember {
                if let Err(e) = consent::store().set(&tool_id, decision) {
                    return Reply::err_msg("io_error", e);
                }
            } else {
                consent::store().set_one_time(&tool_id, decision);
            }
            consent::resolve_pending_for_tool(&tool_id, Some(resolved_decision));
            Reply::ok(consent_snapshot_value())
        }
        Err(e) => Reply::err_msg("bad_request", e),
    }
}

pub(super) async fn consent_stream_pending_impl(payload: Value, sender: StreamSender) {
    // Heartbeat keeps the pipe stream warm so the GUI's HTTP idle
    // timeout can't time out a long-lived "no pending prompts"
    // session. Configurable for tests; default matches the Wylde user's
    // GUI-side keepalive.
    let heartbeat_secs = payload
        .get("heartbeat_secs")
        .and_then(Value::as_u64)
        .filter(|s| *s > 0)
        .unwrap_or(30);
    let heartbeat = std::time::Duration::from_secs(heartbeat_secs);

    let (mut rx, snapshot) = consent::subscribe_pending();

    // Emit existing pending entries first so a tab that opens after a
    // prompt fired still sees the toast.
    for entry in snapshot {
        if sender.send(Ok(pending_event_chunk(&entry))).await.is_err() {
            return;
        }
    }

    loop {
        tokio::select! {
            biased;
            _ = sender.closed() => {
                // Client dropped the receiver — exit cleanly so the
                // broadcast subscription drops with the task.
                return;
            }
            ev = rx.recv() => {
                match ev {
                    Ok(consent::ConsentEvent::Pending(entry)) => {
                        if sender.send(Ok(pending_event_chunk(&entry))).await.is_err() {
                            return;
                        }
                    }
                    Ok(consent::ConsentEvent::Resolved { id, tool, decision }) => {
                        let chunk = json!({
                            "type": "resolved",
                            "id": id,
                            "tool": tool,
                            "decision": decision,
                        });
                        if sender.send(Ok(chunk)).await.is_err() {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Buffer overran — the GUI will refetch the
                        // full pending list via `consent.list` so
                        // skipping this chunk is safe. Tell the
                        // client so it knows to recover.
                        let chunk = json!({"type": "lagged"});
                        if sender.send(Ok(chunk)).await.is_err() {
                            return;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        // Process-wide broadcaster never closes
                        // (static OnceLock), but exit cleanly anyway.
                        return;
                    }
                }
            }
            _ = tokio::time::sleep(heartbeat) => {
                let chunk = json!({
                    "type": "heartbeat",
                    "ts": chrono::Utc::now().timestamp(),
                });
                if sender.send(Ok(chunk)).await.is_err() {
                    return;
                }
            }
        }
    }
}

fn pending_event_chunk(entry: &consent::PendingEntry) -> Value {
    json!({
        "type": "pending",
        "id": entry.id,
        "tool": entry.tool,
        "summary": entry.summary,
        "default_action": entry.default_action,
        "awaiting_since": entry.awaiting_since,
    })
}

#[cfg(test)]
mod tests {
    use serde_json::{json, Value};

    use crate::api::{DefaultHarnessApi, HarnessApi};

    // ── consent.* unit tests (Phase 12.2) ────────────────────────────
    //
    // All consent tests acquire the shared serial guard and write to a
    // tempdir-backed store so they don't corrupt the host's real
    // `data/preferences/consent.json` or race against the gate
    // integration tests in `tooling::runner::tests`.

    async fn consent_test_scope<F, Fut>(test_body: F)
    where
        F: FnOnce() -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        let _g = crate::tooling::consent::serial_test_guard().await;
        // Bypass irrelevant here — we're testing the consent.* verbs
        // themselves, not the gate they affect — but we still need to
        // ensure the global store is pointed at a tempdir so the
        // writes don't persist to the host.
        let td = tempfile::TempDir::new().expect("tempdir");
        crate::tooling::consent::store_set_path_for_tests_helper(
            crate::tooling::consent::store(),
            td.path().join("consent.json"),
        );
        crate::tooling::consent::store()
            .reset()
            .expect("reset injected store");
        test_body().await;
    }

    #[tokio::test]
    async fn consent_list_returns_empty_shape_on_fresh_install() {
        consent_test_scope(|| async {
            let api = DefaultHarnessApi;
            let reply = api.consent_list(Value::Null).await;
            assert!(reply.ok);
            assert_eq!(reply.data["no_auth"], false);
            assert_eq!(
                reply.data["tools"].as_object().unwrap().len(),
                0,
                "fresh store has no per-tool decisions"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn consent_set_persists_and_shows_in_list() {
        consent_test_scope(|| async {
            let api = DefaultHarnessApi;
            let _ = api
                .consent_set(json!({"tool_id": "fs.write_file", "decision": "approved"}))
                .await;
            let listed = api.consent_list(Value::Null).await;
            assert_eq!(listed.data["tools"]["fs.write_file"], "approved");
        })
        .await;
    }

    #[tokio::test]
    async fn consent_set_rejects_unknown_decision() {
        consent_test_scope(|| async {
            let api = DefaultHarnessApi;
            let reply = api
                .consent_set(json!({"tool_id": "fs.write_file", "decision": "maybe"}))
                .await;
            assert!(!reply.ok);
            assert_eq!(reply.error.unwrap().code, "bad_request");
        })
        .await;
    }

    #[tokio::test]
    async fn consent_set_rejects_missing_tool_id() {
        consent_test_scope(|| async {
            let api = DefaultHarnessApi;
            let reply = api.consent_set(json!({"decision": "approved"})).await;
            assert!(!reply.ok);
            assert_eq!(reply.error.unwrap().code, "bad_request");
        })
        .await;
    }

    #[tokio::test]
    async fn consent_respond_writes_through_same_path_as_set() {
        consent_test_scope(|| async {
            let api = DefaultHarnessApi;
            let _ = api
                .consent_respond(json!({"tool_id": "fs.read_file", "decision": "denied"}))
                .await;
            let listed = api.consent_list(Value::Null).await;
            assert_eq!(listed.data["tools"]["fs.read_file"], "denied");
        })
        .await;
    }

    #[tokio::test]
    async fn consent_clear_returns_tool_to_pending() {
        consent_test_scope(|| async {
            let api = DefaultHarnessApi;
            let _ = api
                .consent_set(json!({"tool_id": "fs.write_file", "decision": "approved"}))
                .await;
            let _ = api.consent_clear(json!({"tool_id": "fs.write_file"})).await;
            let listed = api.consent_list(Value::Null).await;
            assert!(listed.data["tools"].as_object().unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test]
    async fn consent_set_no_auth_flips_global_flag() {
        consent_test_scope(|| async {
            let api = DefaultHarnessApi;
            let _ = api.consent_set_no_auth(json!({"enabled": true})).await;
            let listed = api.consent_list(Value::Null).await;
            assert_eq!(listed.data["no_auth"], true);
            let _ = api.consent_set_no_auth(json!({"enabled": false})).await;
            let listed2 = api.consent_list(Value::Null).await;
            assert_eq!(listed2.data["no_auth"], false);
        })
        .await;
    }

    #[tokio::test]
    async fn consent_set_no_auth_rejects_non_bool() {
        consent_test_scope(|| async {
            let api = DefaultHarnessApi;
            let reply = api.consent_set_no_auth(json!({"enabled": "yes"})).await;
            assert!(!reply.ok);
            assert_eq!(reply.error.unwrap().code, "bad_request");
        })
        .await;
    }

    #[tokio::test]
    async fn consent_reset_clears_everything() {
        consent_test_scope(|| async {
            let api = DefaultHarnessApi;
            let _ = api
                .consent_set(json!({"tool_id": "alpha", "decision": "approved"}))
                .await;
            let _ = api.consent_set_no_auth(json!({"enabled": true})).await;
            let _ = api.consent_reset(Value::Null).await;
            let listed = api.consent_list(Value::Null).await;
            assert_eq!(listed.data["no_auth"], false);
            assert!(listed.data["tools"].as_object().unwrap().is_empty());
        })
        .await;
    }

    // ── Phase 12.6: one-time grants + streaming ──────────────────────

    #[tokio::test]
    async fn consent_set_with_remember_true_writes_to_disk() {
        consent_test_scope(|| async {
            let api = DefaultHarnessApi;
            // Default remember is true; explicit `remember: true` is
            // identical.
            let _ = api
                .consent_set(json!({
                    "tool_id": "fs.write_file",
                    "decision": "approved",
                    "remember": true,
                }))
                .await;
            let snap = crate::tooling::consent::store().snapshot();
            assert_eq!(
                snap.tools.get("fs.write_file"),
                Some(&crate::tooling::consent::Decision::Approved),
                "remember:true persists to the file shape"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn consent_set_with_remember_false_does_not_persist() {
        consent_test_scope(|| async {
            let api = DefaultHarnessApi;
            let reply = api
                .consent_set(json!({
                    "tool_id": "fs.write_file",
                    "decision": "approved",
                    "remember": false,
                }))
                .await;
            assert!(reply.ok, "one-time grant returns ok");
            let snap = crate::tooling::consent::store().snapshot();
            assert!(
                !snap.tools.contains_key("fs.write_file"),
                "remember:false must NOT write to disk; got {:?}",
                snap.tools
            );
            // The consent.list reply mirrors the on-disk shape, so
            // the per-tool map should be empty too.
            let listed = api.consent_list(Value::Null).await;
            assert!(
                listed.data["tools"]
                    .as_object()
                    .map(|m| m.is_empty())
                    .unwrap_or(false),
                "consent.list should show no per-tool entries"
            );
        })
        .await;
    }

    #[tokio::test]
    async fn consent_set_with_remember_false_resolves_current_call_then_returns_pending() {
        consent_test_scope(|| async {
            let api = DefaultHarnessApi;
            let _ = api
                .consent_set(json!({
                    "tool_id": "fs.write_file",
                    "decision": "approved",
                    "remember": false,
                }))
                .await;
            // The one-time approval is consumed by the next gate
            // check.
            let outcome = crate::tooling::consent::store().check("fs.write_file", || "p".into());
            assert_eq!(outcome, crate::tooling::consent::GateOutcome::Allow);
            // Second check → no record → pending.
            let outcome2 = crate::tooling::consent::store().check("fs.write_file", || "p".into());
            assert!(matches!(
                outcome2,
                crate::tooling::consent::GateOutcome::Pending { .. }
            ));
        })
        .await;
    }

    #[tokio::test]
    async fn consent_set_rejects_non_bool_remember() {
        consent_test_scope(|| async {
            let api = DefaultHarnessApi;
            let reply = api
                .consent_set(json!({
                    "tool_id": "fs.write_file",
                    "decision": "approved",
                    "remember": "no",
                }))
                .await;
            assert!(!reply.ok);
            assert_eq!(reply.error.unwrap().code, "bad_request");
        })
        .await;
    }

    // ── consent.stream_pending streaming tests ───────────────────────

    /// Drain a single pending event from the receiver. Skips any
    /// initial-snapshot frames whose ids don't match `id`.
    async fn next_chunk(
        rx: &mut tokio::sync::mpsc::Receiver<Result<Value, wylde_shared::ipc::IpcError>>,
    ) -> Value {
        let frame = tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("frame arrives within 1s")
            .expect("stream still open");
        frame.expect("chunk is Ok")
    }

    #[tokio::test]
    async fn consent_stream_pending_emits_event_on_record_pending() {
        let _g = crate::tooling::consent::serial_test_guard().await;
        crate::tooling::consent::clear_pending();
        let api = DefaultHarnessApi;
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let handle = tokio::spawn(async move {
            api.consent_stream_pending(json!({"heartbeat_secs": 999}), tx)
                .await;
        });
        // Give the spawned task time to subscribe before we record.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let id = crate::tooling::consent::record_pending(
            "fs.write_file",
            "writes a file".into(),
            "deny",
        );
        let chunk = next_chunk(&mut rx).await;
        assert_eq!(chunk["type"], "pending");
        assert_eq!(chunk["id"], id);
        assert_eq!(chunk["tool"], "fs.write_file");
        assert_eq!(chunk["default_action"], "deny");
        assert!(chunk["summary"].is_string());
        assert!(chunk["awaiting_since"].is_i64());
        // Drop the receiver — handler should exit cleanly.
        drop(rx);
        tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("handler exits within 1s after receiver drops")
            .expect("join ok");
        crate::tooling::consent::clear_pending();
    }

    #[tokio::test]
    async fn consent_stream_pending_emits_resolved_on_consent_set() {
        let _g = crate::tooling::consent::serial_test_guard().await;
        crate::tooling::consent::clear_pending();
        let td = tempfile::TempDir::new().expect("tempdir");
        crate::tooling::consent::store_set_path_for_tests_helper(
            crate::tooling::consent::store(),
            td.path().join("consent.json"),
        );
        crate::tooling::consent::store().reset().expect("reset");

        let api = DefaultHarnessApi;
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let handle = tokio::spawn(async move {
            api.consent_stream_pending(json!({"heartbeat_secs": 999}), tx)
                .await;
        });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        let id = crate::tooling::consent::record_pending(
            "fs.write_file",
            "writes a file".into(),
            "deny",
        );
        // Drain pending event.
        let pending = next_chunk(&mut rx).await;
        assert_eq!(pending["type"], "pending");

        let _ = api
            .consent_set(json!({
                "tool_id": "fs.write_file",
                "decision": "approved",
            }))
            .await;
        let resolved = next_chunk(&mut rx).await;
        assert_eq!(resolved["type"], "resolved");
        assert_eq!(resolved["id"], id);
        assert_eq!(resolved["tool"], "fs.write_file");
        assert_eq!(resolved["decision"], "approved");

        drop(rx);
        tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("handler exits within 1s")
            .expect("join ok");
        crate::tooling::consent::clear_pending();
    }

    #[tokio::test]
    async fn consent_stream_pending_emits_snapshot_on_subscribe() {
        let _g = crate::tooling::consent::serial_test_guard().await;
        crate::tooling::consent::clear_pending();
        let id = crate::tooling::consent::record_pending(
            "fs.write_file",
            "writes a file".into(),
            "deny",
        );
        let api = DefaultHarnessApi;
        let (tx, mut rx) = tokio::sync::mpsc::channel(16);
        let handle = tokio::spawn(async move {
            api.consent_stream_pending(json!({"heartbeat_secs": 999}), tx)
                .await;
        });
        let chunk = next_chunk(&mut rx).await;
        assert_eq!(chunk["type"], "pending");
        assert_eq!(chunk["id"], id);
        drop(rx);
        tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("handler exits within 1s")
            .expect("join ok");
        crate::tooling::consent::clear_pending();
    }

    #[tokio::test]
    async fn consent_stream_pending_closes_cleanly_on_client_drop() {
        let _g = crate::tooling::consent::serial_test_guard().await;
        crate::tooling::consent::clear_pending();
        let api = DefaultHarnessApi;
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let handle = tokio::spawn(async move {
            api.consent_stream_pending(json!({"heartbeat_secs": 999}), tx)
                .await;
        });
        // Give the spawn time to enter its select loop.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        // Client disconnects.
        drop(rx);
        // Handler must observe sender.closed() and exit.
        tokio::time::timeout(std::time::Duration::from_secs(1), handle)
            .await
            .expect("handler exits within 1s after client drops")
            .expect("join ok");
    }
}
