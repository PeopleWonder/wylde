//! Live Memgraph integration test.
//!
//! Drives the Rust memgraph client against a real `wylde-memgraph`
//! service over the named pipe. Asserts the contract the Wylde user's brief
//! cares about: "memgraph is operational with the RAG" — i.e. the
//! client can complete a real round-trip and the routes return
//! sensible envelopes.
//!
//! Why `#[ignore]`: depends on the `wylde-memgraph` service (and the
//! Neo4j child it spawns) being live on `\\.\pipe\wylde-memgraph`. CI
//! and a cold checkout lack both; the offline unit tests under
//! `src/memory/memgraph/` cover every code path against a mock
//! transport. Same pattern as the Phase 11.A/B Whisper / Kokoro
//! end-to-end tests.
//!
//! Run with:
//!
//! ```
//! cargo test -p wylde-harness --test memgraph_integration -- --ignored --nocapture
//! ```
//!
//! Optional: override the service via `WYLDE_MEMGRAPH_SERVICE` if your
//! deployment doesn't use the default `wylde-memgraph` pipe name.

use std::time::Duration;

use wylde_harness::memory::memgraph::client::{Client, TraverseRequest};

/// Best-effort smoke test against a running Memgraph. Pins the
/// minimum behaviour the RAG port will rely on: a live pipe responds
/// to `/health`, `/stats`, and `/traverse` without panicking, and the
/// reply envelopes carry the expected JSON keys.
#[tokio::test]
#[ignore = "requires the wylde-memgraph service running on \\\\.\\pipe\\wylde-memgraph"]
async fn smoke_health_stats_traverse() {
    // The shared IPC client respects `WYLDE_IPC_DISABLE` — keep it
    // unset for this test (it's the env knob that makes the Rust IPC
    // refuse to dispatch in offline test runs). If a sibling test
    // happens to have set it, clear it for the duration of this call.
    // SAFETY: single-threaded test setup; no readers.
    unsafe {
        std::env::remove_var("WYLDE_IPC_DISABLE");
    }

    let client = Client::new().with_timeout(Duration::from_secs(10));
    eprintln!("integration: targeting service `{}`", client.service());

    // ── /health ──────────────────────────────────────────────────────
    let health = client.health().await;
    assert!(
        health.ok,
        "GET /health should succeed against a running memgraph; err = {:?}",
        health.error
    );

    // ── /stats ───────────────────────────────────────────────────────
    let stats = client.stats().await;
    assert!(
        stats.ok,
        "GET /stats should succeed; err = {:?}",
        stats.error
    );
    // Pin the field names downstream consumers (rag_graph_stats) rely on.
    for key in ["entities", "chunks", "mentions"] {
        assert!(
            stats.data.get(key).is_some(),
            "stats envelope missing `{key}`: {}",
            stats.data
        );
    }

    // ── /traverse ────────────────────────────────────────────────────
    // Use a synthetic entity name guaranteed not to exist — the route
    // should still respond `ok=true` with an empty chunks list, NOT
    // explode. Pinning the empty-result envelope shape is what the
    // graph_retrieval fallback relies on.
    let nonce = format!("rust_integration_nonce_{}", uuid::Uuid::new_v4().simple());
    let req = TraverseRequest::for_entities(vec![nonce.clone()]);
    let trv = client.traverse(req).await;
    assert!(
        trv.ok,
        "POST /traverse should respond ok for unknown entities (empty result); err = {:?}",
        trv.error
    );
    // The route emits `{"ok": true, "chunks": []}`; the IPC layer
    // strips `ok` (it becomes `Reply::ok`) so `data` is the inner
    // body. `chunks` may be empty but must be present.
    let chunks = trv
        .data
        .get("chunks")
        .and_then(serde_json::Value::as_array)
        .unwrap_or_else(|| panic!("traverse reply missing `chunks` array: {}", trv.data));
    eprintln!(
        "integration: traverse(nonce={nonce}) returned {} chunk(s) (expected 0)",
        chunks.len()
    );
}

/// Connection-refused smoke: when the service is NOT running, the
/// client must return a structured `pipe_connect` reply, not panic.
/// This one isn't `#[ignore]` — it deliberately targets a known-dead
/// pipe so it runs on every test invocation.
#[tokio::test]
async fn connection_refused_returns_pipe_connect_error() {
    let prev = std::env::var("WYLDE_MEMGRAPH_SERVICE").ok();
    let prev_disable = std::env::var("WYLDE_IPC_DISABLE").ok();
    // SAFETY: single-threaded test setup; no readers.
    unsafe {
        std::env::set_var(
            "WYLDE_MEMGRAPH_SERVICE",
            format!("missing-memgraph-{}", uuid::Uuid::new_v4().simple()),
        );
        std::env::remove_var("WYLDE_IPC_DISABLE");
    }

    let client = Client::new().with_timeout(Duration::from_millis(500));
    let reply = client.health().await;

    assert!(!reply.ok, "expected error reply, got: {reply:?}");
    let err = reply.error.expect("error body");
    // On Windows we hit pipe_connect; on non-Windows we hit pipe_unavailable.
    assert!(
        err.code == "pipe_connect" || err.code == "pipe_unavailable",
        "unexpected error code: {}",
        err.code
    );

    // SAFETY: single-threaded test teardown.
    unsafe {
        match prev {
            Some(v) => std::env::set_var("WYLDE_MEMGRAPH_SERVICE", v),
            None => std::env::remove_var("WYLDE_MEMGRAPH_SERVICE"),
        }
        match prev_disable {
            Some(v) => std::env::set_var("WYLDE_IPC_DISABLE", v),
            None => std::env::remove_var("WYLDE_IPC_DISABLE"),
        }
    }
}
