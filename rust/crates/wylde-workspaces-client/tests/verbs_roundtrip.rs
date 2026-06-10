//! Per-verb coverage for the Slice 0b client surface.
//!
//! Two halves:
//!   1. **Verb table** — every relocated `workspaces.*` verb is registered in
//!      the [`verbs`] table with the timeout / retry / cache policy from Build
//!      Order Appendix A (a pure lookup, no I/O).
//!   2. **Mock round-trip** — each typed wrapper sends the right action +
//!      payload and parses the reply, driven against a `wylde-shared` pipe
//!      server with canned handlers (no real `wylde-workspaces` service).
//!
//! Windows-only — IPC uses named pipes.

#![cfg(windows)]

use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};
use wylde_shared::ipc;
use wylde_workspaces_client::retry::RetryPolicy;
use wylde_workspaces_client::timeouts::{TimeoutPolicy, FAST, MEDIUM, SLOW};
use wylde_workspaces_client::verbs;
use wylde_workspaces_client::WorkspacesClient;

// ── 1. Verb table ────────────────────────────────────────────────────────

#[test]
fn slice_0b_verbs_have_expected_policies() {
    // (verb, expected timeout, has-retry, cache_ttl secs)
    let expected: &[(&str, TimeoutPolicy, bool, Option<u64>)] = &[
        (
            "workspaces.list_mru",
            TimeoutPolicy::Fixed(FAST),
            true,
            Some(30),
        ),
        (
            "workspaces.set_active",
            TimeoutPolicy::Fixed(FAST),
            true,
            None,
        ),
        ("workspaces.create", TimeoutPolicy::Fixed(SLOW), false, None),
        ("workspaces.update", TimeoutPolicy::Fixed(FAST), true, None),
        (
            "workspaces.delete",
            TimeoutPolicy::Fixed(MEDIUM),
            false,
            None,
        ),
        (
            "workspaces.set_persona",
            TimeoutPolicy::Fixed(FAST),
            true,
            None,
        ),
        (
            "workspaces.rag_query",
            TimeoutPolicy::Fixed(SLOW),
            true,
            None,
        ),
        (
            "workspaces.reindex",
            TimeoutPolicy::Fixed(SLOW),
            false,
            None,
        ),
        // ── Slice B — graph (Plan v2 §7 / Build Order Appendix A) ───────
        // Medium · idempotent read (≤4) · 5s cache TTL.
        (
            "workspaces.graph",
            TimeoutPolicy::Fixed(MEDIUM),
            true,
            Some(5),
        ),
        // ── Slice 0c — notes (Build Order Appendix A) ──────────────────
        (
            "workspaces.notes.list",
            TimeoutPolicy::Fixed(MEDIUM),
            true,
            None,
        ),
        (
            "workspaces.notes.add",
            TimeoutPolicy::Fixed(MEDIUM),
            true,
            None,
        ),
        (
            "workspaces.notes.update",
            TimeoutPolicy::Fixed(MEDIUM),
            true,
            None,
        ),
        (
            "workspaces.notes.delete",
            TimeoutPolicy::Fixed(MEDIUM),
            false,
            None,
        ),
        (
            "workspaces.notes.search",
            TimeoutPolicy::Fixed(MEDIUM),
            true,
            None,
        ),
        (
            "workspaces.notes.propose",
            TimeoutPolicy::Fixed(MEDIUM),
            false,
            None,
        ),
        // ── Slice 0c — workspace conversations (shape-assigned) ────────
        (
            "workspaces.conversations.list",
            TimeoutPolicy::Fixed(MEDIUM),
            true,
            None,
        ),
        (
            "workspaces.conversations.get",
            TimeoutPolicy::Fixed(MEDIUM),
            true,
            None,
        ),
        (
            "workspaces.conversations.delete",
            TimeoutPolicy::Fixed(MEDIUM),
            false,
            None,
        ),
    ];
    for (name, timeout, has_retry, ttl) in expected {
        let def = verbs::lookup(name).unwrap_or_else(|| panic!("{name} missing from verb table"));
        assert_eq!(def.timeout, *timeout, "{name} timeout");
        let retries = !matches!(def.retry, RetryPolicy::NoRetry);
        assert_eq!(retries, *has_retry, "{name} retry shape");
        assert_eq!(def.cache_ttl.map(|d| d.as_secs()), *ttl, "{name} cache ttl");
    }
}

// ── 2. Mock round-trip ───────────────────────────────────────────────────

fn unique_service_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!(
        "wylde-workspaces-clienttest-{}-{}",
        std::process::id(),
        nanos
    )
}

/// One recorded (verb, payload) the mock server saw.
type Calls = Arc<Mutex<Vec<(String, Value)>>>;

fn register_recording(verb: &'static str, calls: &Calls, reply: Value) {
    let calls = Arc::clone(calls);
    ipc::register_action(verb, move |payload: Value| {
        let calls = Arc::clone(&calls);
        let reply = reply.clone();
        async move {
            calls.lock().unwrap().push((verb.to_string(), payload));
            ipc::Reply::ok(reply)
        }
    });
}

#[tokio::test]
async fn wrappers_send_correct_action_and_payload_and_parse_reply() {
    let service = unique_service_name();
    let calls: Calls = Arc::new(Mutex::new(Vec::new()));

    // Canned replies per verb (shape mirrors `wylde_workspaces::api`).
    register_recording(
        "workspaces.list_mru",
        &calls,
        json!({"workspaces": [], "active_id": null}),
    );
    register_recording(
        "workspaces.set_active",
        &calls,
        json!({"active_id": "w-1", "mru": ["w-1"]}),
    );
    register_recording(
        "workspaces.create",
        &calls,
        json!({"id": "w-1", "name": "P"}),
    );
    register_recording(
        "workspaces.update",
        &calls,
        json!({"id": "w-1", "rag_enabled": false}),
    );
    register_recording(
        "workspaces.delete",
        &calls,
        json!({"ok": true, "workspace_id": "w-1"}),
    );
    register_recording(
        "workspaces.set_persona",
        &calls,
        json!({"ok": true, "workspace_id": "w-1"}),
    );
    register_recording(
        "workspaces.rag_query",
        &calls,
        json!({"workspace_id": "w-1", "hits": []}),
    );
    register_recording(
        "workspaces.reindex",
        &calls,
        json!({"ok": true, "file_count": 0}),
    );

    // Stand up the mock pipe server.
    let server = Arc::new(ipc::PipeServer::new(&service));
    let server_clone = Arc::clone(&server);
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("server runtime");
        let _ = rt.block_on(server_clone.accept_loop());
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let client = WorkspacesClient::for_service(&service);

    // Each wrapper round-trips and parses its canned reply.
    assert_eq!(
        client.create("/proj", Some("P")).await.unwrap()["id"],
        "w-1"
    );
    assert_eq!(client.set_active("w-1").await.unwrap()["active_id"], "w-1");
    assert_eq!(
        client
            .update(json!({"workspace_id": "w-1", "rag_enabled": false}))
            .await
            .unwrap()["rag_enabled"],
        false
    );
    assert_eq!(
        client.set_persona("w-1", "Be terse.").await.unwrap()["ok"],
        true
    );
    assert_eq!(
        client.rag_query("w-1", "q", Some(5)).await.unwrap()["hits"],
        json!([])
    );
    assert_eq!(client.reindex("w-1").await.unwrap()["file_count"], 0);
    assert_eq!(client.delete("w-1").await.unwrap()["ok"], true);
    assert_eq!(client.list_mru().await.unwrap()["active_id"], Value::Null);

    // Spot-check the payloads the wrappers built.
    let recorded = calls.lock().unwrap().clone();
    let payload_for = |verb: &str| -> Value {
        recorded
            .iter()
            .find(|(v, _)| v == verb)
            .map(|(_, p)| p.clone())
            .unwrap()
    };
    assert_eq!(
        payload_for("workspaces.create"),
        json!({"folder": "/proj", "name": "P"})
    );
    assert_eq!(
        payload_for("workspaces.set_active"),
        json!({"workspace_id": "w-1"})
    );
    assert_eq!(
        payload_for("workspaces.set_persona"),
        json!({"workspace_id": "w-1", "text": "Be terse."})
    );
    assert_eq!(
        payload_for("workspaces.rag_query"),
        json!({"workspace_id": "w-1", "query": "q", "k": 5})
    );
    assert_eq!(
        payload_for("workspaces.reindex"),
        json!({"workspace_id": "w-1"})
    );

    // ── cache: a second list_mru is served from the 30s TTL cache, so the
    //    mock server is hit exactly once for that verb. ──────────────────
    let _ = client.list_mru().await.unwrap();
    let list_calls = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(v, _)| v == "workspaces.list_mru")
        .count();
    assert_eq!(
        list_calls, 1,
        "list_mru should be cached after the first call"
    );

    // Cleanup the process-wide action registry.
    for v in [
        "workspaces.list_mru",
        "workspaces.set_active",
        "workspaces.create",
        "workspaces.update",
        "workspaces.delete",
        "workspaces.set_persona",
        "workspaces.rag_query",
        "workspaces.reindex",
    ] {
        ipc::unregister_action(v);
    }
}

#[tokio::test]
async fn slice_0c_wrappers_send_correct_action_and_payload() {
    let service = unique_service_name();
    let calls: Calls = Arc::new(Mutex::new(Vec::new()));

    register_recording(
        "workspaces.notes.list",
        &calls,
        json!({"workspace_id": "w-1", "notes": [], "count": 0}),
    );
    register_recording(
        "workspaces.notes.add",
        &calls,
        json!({"id": "n-1", "text": "t"}),
    );
    register_recording(
        "workspaces.notes.update",
        &calls,
        json!({"id": "n-1", "text": "t2"}),
    );
    register_recording(
        "workspaces.notes.delete",
        &calls,
        json!({"ok": true, "id": "n-1"}),
    );
    register_recording(
        "workspaces.notes.search",
        &calls,
        json!({"workspace_id": "w-1", "notes": [], "count": 0}),
    );
    register_recording(
        "workspaces.notes.propose",
        &calls,
        json!({"candidate": {"id": "n-2", "text": "t"}}),
    );
    register_recording(
        "workspaces.conversations.list",
        &calls,
        json!({"workspace_id": "w-1", "conversations": [], "count": 0}),
    );
    register_recording(
        "workspaces.conversations.get",
        &calls,
        json!({"id": "c-1", "title": "T"}),
    );
    register_recording(
        "workspaces.conversations.delete",
        &calls,
        json!({"ok": true, "id": "c-1"}),
    );

    let server = Arc::new(ipc::PipeServer::new(&service));
    let server_clone = Arc::clone(&server);
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("server runtime");
        let _ = rt.block_on(server_clone.accept_loop());
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let client = WorkspacesClient::for_service(&service);

    assert_eq!(client.notes_list("w-1").await.unwrap()["count"], 0);
    assert_eq!(client.notes_add("w-1", "t").await.unwrap()["id"], "n-1");
    assert_eq!(
        client.notes_update("w-1", "n-1", "t2").await.unwrap()["text"],
        "t2"
    );
    assert_eq!(client.notes_delete("w-1", "n-1").await.unwrap()["ok"], true);
    assert_eq!(
        client.notes_search("w-1", "q", Some(3)).await.unwrap()["count"],
        0
    );
    assert_eq!(
        client.notes_propose("w-1", "t").await.unwrap()["candidate"]["id"],
        "n-2"
    );
    assert_eq!(client.conversations_list("w-1").await.unwrap()["count"], 0);
    assert_eq!(
        client.conversations_get("w-1", "c-1").await.unwrap()["title"],
        "T"
    );
    assert_eq!(
        client.conversations_delete("w-1", "c-1").await.unwrap()["ok"],
        true
    );

    let recorded = calls.lock().unwrap().clone();
    let payload_for = |verb: &str| -> Value {
        recorded
            .iter()
            .find(|(v, _)| v == verb)
            .map(|(_, p)| p.clone())
            .unwrap()
    };
    assert_eq!(
        payload_for("workspaces.notes.add"),
        json!({"workspace_id": "w-1", "text": "t"})
    );
    assert_eq!(
        payload_for("workspaces.notes.update"),
        json!({"workspace_id": "w-1", "id": "n-1", "text": "t2"})
    );
    assert_eq!(
        payload_for("workspaces.notes.search"),
        json!({"workspace_id": "w-1", "query": "q", "limit": 3})
    );
    assert_eq!(
        payload_for("workspaces.notes.propose"),
        json!({"workspace_id": "w-1", "text": "t"})
    );
    assert_eq!(
        payload_for("workspaces.conversations.get"),
        json!({"workspace_id": "w-1", "id": "c-1"})
    );
    assert_eq!(
        payload_for("workspaces.conversations.delete"),
        json!({"workspace_id": "w-1", "id": "c-1"})
    );

    for v in [
        "workspaces.notes.list",
        "workspaces.notes.add",
        "workspaces.notes.update",
        "workspaces.notes.delete",
        "workspaces.notes.search",
        "workspaces.notes.propose",
        "workspaces.conversations.list",
        "workspaces.conversations.get",
        "workspaces.conversations.delete",
    ] {
        ipc::unregister_action(v);
    }
}

// ── 3. Slice B — graph wrapper round-trip + 5s cache ──────────────────────

#[tokio::test]
async fn slice_b_graph_wrapper_roundtrips_and_is_cached() {
    let service = unique_service_name();
    let calls: Calls = Arc::new(Mutex::new(Vec::new()));

    // A minimal WorkspaceGraph-shaped reply (the real shape is asserted in
    // the service's projection unit tests; here we only exercise the client).
    let graph_reply = json!({
        "nodes": [{
            "id": "alpha", "kind": "Function", "name": "alpha",
            "file": "src/a.rs", "line": 0,
            "position": {"x": 0.0, "y": 0.0, "z": 0.0}, "style": {}
        }],
        "edges": [{"src": "alpha", "dst": "beta", "rel_type": "CALLS", "weight": 1.0}],
        "clusters": []
    });
    register_recording("workspaces.graph", &calls, graph_reply);

    let server = Arc::new(ipc::PipeServer::new(&service));
    let server_clone = Arc::clone(&server);
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("server runtime");
        let _ = rt.block_on(server_clone.accept_loop());
    });
    tokio::time::sleep(Duration::from_millis(300)).await;

    let client = WorkspacesClient::for_service(&service);

    // First call round-trips and parses the canned reply.
    let g = client.graph("w-1").await.unwrap();
    assert_eq!(g["nodes"][0]["id"], "alpha");
    assert_eq!(g["edges"][0]["rel_type"], "CALLS");

    // The wrapper sends the documented payload.
    let recorded = calls.lock().unwrap().clone();
    assert_eq!(recorded[0].1, json!({"workspace_id": "w-1"}));

    // A second call within the 5s TTL is served from cache — the mock server
    // is hit exactly once for `workspaces.graph`.
    let _ = client.graph("w-1").await.unwrap();
    let hits = calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(v, _)| v == "workspaces.graph")
        .count();
    assert_eq!(
        hits, 1,
        "graph should be served from the 5s cache on the 2nd call"
    );

    ipc::unregister_action("workspaces.graph");
}
