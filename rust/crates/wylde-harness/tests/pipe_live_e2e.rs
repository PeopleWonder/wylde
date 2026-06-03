//! End-to-end pipe-action tests for Phase 9.
//!
//! Spin up a real `PipeServer` on a uniquely-named pipe, register the
//! `wylde-harness` action surface via [`wylde_harness::pipe::install_all`],
//! and exercise each verb category over the wire. Pins the
//! serialize → deserialize round-trip and the verb dispatch table
//! together (a missing registration in `pipe/mod.rs` trips here as
//! `no_action`).
//!
//! Windows-only — IPC uses named pipes. Non-Windows builds compile but
//! every test is a no-op.

#![cfg(windows)]

use std::sync::Arc;
use std::time::Duration;

use serde_json::json;
use tokio::sync::{Mutex as AsyncMutex, MutexGuard};
use wylde_harness::pipe;
use wylde_shared::ipc;

/// Serialise tests that share the process-wide action registry. The
/// IPC action map is global; without this guard two `tokio::test`s
/// register and collide when one's call hits the other's mock.
async fn registry_guard() -> MutexGuard<'static, ()> {
    static LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
    LOCK.lock().await
}

/// Spin up a fresh per-test `wylde-harness` pipe with all pipe verbs
/// registered. Returns the service name + a handle that stops the
/// server on drop.
async fn spin_up_pipe() -> (String, Arc<ipc::PipeServer>, tokio::task::JoinHandle<anyhow::Result<()>>) {
    let suffix = uuid::Uuid::new_v4().simple().to_string();
    let service = format!("wylde-harness-pipe-test-{suffix}");
    // Re-register the full pipe surface — actions are global, so this
    // overwrites any prior registration with the same names. Tests that
    // share the lock are sequential, so this is safe.
    pipe::install_all();
    let server = Arc::new(ipc::PipeServer::new(&service));
    let server_clone = Arc::clone(&server);
    let task = tokio::spawn(async move { server_clone.accept_loop().await });
    tokio::time::sleep(Duration::from_millis(100)).await;
    (service, server, task)
}

#[tokio::test]
async fn tools_list_over_live_pipe_returns_catalog() {
    let _g = registry_guard().await;
    let (service, server, task) = spin_up_pipe().await;

    let reply = ipc::send_action(&service, "tools.list", json!(null)).await;
    assert!(reply.ok, "tools.list reply not ok: {reply:?}");
    let tools = reply.data["tools"].as_array().expect("tools is array");
    assert!(!tools.is_empty(), "catalog must have at least one entry");
    let count = reply.data["count"].as_u64().expect("count is uint");
    assert_eq!(count as usize, tools.len());

    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
}

#[tokio::test]
async fn tools_run_over_live_pipe_invokes_time_now() {
    let _g = registry_guard().await;
    // Phase 12.2 consent gate guards every dispatch; bypass it for
    // the e2e wire-shape test. The gate itself is exercised by
    // `tooling::runner::tests` (unit) and `consent::tests` (store).
    let _cg = wylde_harness::tooling::consent::serial_test_guard().await;
    wylde_harness::tooling::consent::set_bypass_for_tests(true);
    let (service, server, task) = spin_up_pipe().await;

    let reply = ipc::send_action(&service, "tools.run", json!({"name": "time.now"})).await;
    assert!(reply.ok, "tools.run reply not ok: {reply:?}");
    // Inner envelope is `{ok, data, canonical_id, elapsed_ms}`.
    assert_eq!(reply.data["ok"], true);
    assert_eq!(reply.data["canonical_id"], "time_now");
    assert_eq!(reply.data["data"]["status"], "success");

    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
}

#[tokio::test]
async fn tools_run_over_live_pipe_returns_not_found_for_unknown() {
    let _g = registry_guard().await;
    let (service, server, task) = spin_up_pipe().await;

    let reply = ipc::send_action(
        &service,
        "tools.run",
        json!({"name": "definitely.not.a.tool"}),
    )
    .await;
    // Outer envelope is ok (transport-level success); inner envelope
    // carries the not_found.
    assert!(reply.ok);
    assert_eq!(reply.data["ok"], false);
    assert_eq!(reply.data["error"]["code"], "not_found");

    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
}

#[tokio::test]
async fn unregistered_verb_returns_no_action_for_strangler_fallback() {
    let _g = registry_guard().await;
    let (service, server, task) = spin_up_pipe().await;

    // Verbs the Python pipe handles but the Rust pipe doesn't yet —
    // see `pipe::mod` docs for the punchlist. These MUST surface as
    // `no_action` so the Python strangler's transport-code fallback
    // reverts to in-process Python instead of bricking the call.
    for verb in [
        "memory.workspace.list",
        "memory.short_term.get",
        "memory.reflect",
        "conversations.new",
        "prompts.list",
        "rag.workspaces.list",
        // models.* registry/Ollama verbs are registered as of Slice 3a;
        // the Voice-coupled transcribe/synthesize stay forward-only and
        // must still surface as no_action for the strangler fallback.
        "models.transcribe",
        "models.synthesize",
    ] {
        let reply = ipc::send_action(&service, verb, json!({})).await;
        assert!(
            !reply.ok,
            "unregistered verb {verb:?} unexpectedly returned ok"
        );
        let err = reply.error.as_ref().expect("error envelope present");
        assert_eq!(
            err.code, "no_action",
            "verb {verb:?} returned {:?} (expected no_action for strangler fallback)",
            err.code
        );
    }

    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;
}

#[tokio::test]
async fn memory_long_term_save_then_list_round_trips_over_live_pipe() {
    let _g = registry_guard().await;

    // Per-test tempdir so this run doesn't poison the user's real
    // `<data_dir>/long_term.json`. The subsystem reads the env var on
    // every call, so setting it process-wide for this test is enough —
    // the registry_guard serializes us against other env-var users.
    let td = tempfile::tempdir().expect("tempdir");
    let prior = std::env::var_os("WYLDE_DATA_DIR");
    std::env::set_var("WYLDE_DATA_DIR", td.path());

    let (service, server, task) = spin_up_pipe().await;

    let saved = ipc::send_action(
        &service,
        "memory.long_term.save",
        json!({"body": "wire-level round trip", "source": "pipe_test"}),
    )
    .await;
    assert!(saved.ok, "save reply: {saved:?}");
    let id = saved.data["id"].as_str().expect("id is string").to_owned();
    assert_eq!(saved.data["body"], "wire-level round trip");

    let listed = ipc::send_action(&service, "memory.long_term.list", json!({})).await;
    assert!(listed.ok);
    assert_eq!(listed.data["count"], 1);
    let memories = listed.data["memories"].as_array().expect("array");
    assert_eq!(memories[0]["id"], id);

    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

    match prior {
        Some(v) => std::env::set_var("WYLDE_DATA_DIR", v),
        None => std::env::remove_var("WYLDE_DATA_DIR"),
    }
}

#[tokio::test]
async fn models_default_round_trips_over_live_pipe_when_flag_on() {
    // Slice 3a: the model_state-backed verbs (set_default → get_default)
    // round-trip end-to-end over the wire when the Rust impl flag is on.
    // Persistence is disk-only (no Ollama needed), so this stays
    // self-contained.
    let _g = registry_guard().await;

    let td = tempfile::tempdir().expect("tempdir");
    let prior_impl = std::env::var_os("WYLDE_HARNESS_MODELS_IMPL");
    let prior_active = std::env::var_os("ACTIVE_MODEL_PATH");
    let prior_default = std::env::var_os("DEFAULT_MODEL_PATH");
    let prior_env_default = std::env::var_os("WYLDE_DEFAULT_MODEL");
    std::env::set_var("WYLDE_HARNESS_MODELS_IMPL", "rust");
    std::env::set_var("ACTIVE_MODEL_PATH", td.path().join("active_model.json"));
    std::env::set_var("DEFAULT_MODEL_PATH", td.path().join("default_model.json"));
    std::env::remove_var("WYLDE_DEFAULT_MODEL");
    wylde_harness::model_registry::model_state::reset_for_tests();

    let (service, server, task) = spin_up_pipe().await;

    // Nothing starred yet → null.
    let got = ipc::send_action(&service, "models.get_default", json!({})).await;
    assert!(got.ok, "get_default reply: {got:?}");
    assert_eq!(got.data["model"], serde_json::Value::Null);

    // Star one.
    let set = ipc::send_action(
        &service,
        "models.set_default",
        json!({"model": "qwen3:0.6b"}),
    )
    .await;
    assert!(set.ok, "set_default reply: {set:?}");
    assert_eq!(set.data["ok"], true);
    assert_eq!(set.data["model"], "qwen3:0.6b");

    // Read it back over a fresh request.
    let got = ipc::send_action(&service, "models.get_default", json!({})).await;
    assert!(got.ok);
    assert_eq!(got.data["model"], "qwen3:0.6b");

    // get_profile for an unknown model is an empty profile, not an error.
    let prof = ipc::send_action(
        &service,
        "models.get_profile",
        json!({"name": "qwen3:0.6b"}),
    )
    .await;
    assert!(prof.ok, "get_profile reply: {prof:?}");
    assert_eq!(prof.data["name"], "qwen3:0.6b");

    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

    // Restore env + caches so sibling tests see a clean slate.
    wylde_harness::model_registry::model_state::reset_for_tests();
    restore("WYLDE_HARNESS_MODELS_IMPL", prior_impl);
    restore("ACTIVE_MODEL_PATH", prior_active);
    restore("DEFAULT_MODEL_PATH", prior_default);
    restore("WYLDE_DEFAULT_MODEL", prior_env_default);
}

#[tokio::test]
async fn models_verbs_gated_off_return_not_implemented() {
    // With the flag off (the default), the registered models.* handlers
    // must report `not_implemented` — a transport-class code the Python
    // forward path (Slice 3b) treats as "fall back to Python". This is
    // the explicit-disabled marker that replaces the silent fallback the
    // Slice 3 stop-finding warned about.
    let _g = registry_guard().await;
    let prior_impl = std::env::var_os("WYLDE_HARNESS_MODELS_IMPL");
    std::env::remove_var("WYLDE_HARNESS_MODELS_IMPL");

    let (service, server, task) = spin_up_pipe().await;

    let got = ipc::send_action(&service, "models.get_default", json!({})).await;
    assert!(!got.ok, "gated-off verb unexpectedly ok: {got:?}");
    assert_eq!(got.error.as_ref().unwrap().code, "not_implemented");

    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

    restore("WYLDE_HARNESS_MODELS_IMPL", prior_impl);
}

#[tokio::test]
async fn rag_add_episodic_then_search_round_trips_over_live_pipe() {
    // Wylde_Study S2a: the headline contract — an episodic row written
    // via `rag.add_episodic` is retrievable through a follow-up
    // `rag.search`, end-to-end over the wire. Precomputed vectors keep
    // the test self-contained (no live wylde-ollama embedder needed);
    // the embed path itself is unit-tested in `memory::embeddings`.
    let _g = registry_guard().await;

    let td = tempfile::tempdir().expect("tempdir");
    let prior = std::env::var_os("WYLDE_DATA_DIR");
    std::env::set_var("WYLDE_DATA_DIR", td.path());
    let prior_dim = std::env::var_os("WYLDE_EMBED_DIM");
    std::env::set_var("WYLDE_EMBED_DIM", "4");

    let (service, server, task) = spin_up_pipe().await;

    let added = ipc::send_action(
        &service,
        "rag.add_episodic",
        json!({
            "content": "the optic nerve carries signals from the retina",
            "url": "http://anat.example/eye",
            "vector": [1.0, 0.0, 0.0, 0.0],
        }),
    )
    .await;
    assert!(added.ok, "add_episodic reply: {added:?}");
    assert_eq!(added.data["status"], "ok");
    let id = added.data["memory_id"]
        .as_str()
        .expect("memory_id is string")
        .to_owned();

    let found = ipc::send_action(
        &service,
        "rag.search",
        json!({
            "q": "what carries signals from the retina",
            "query_vector": [1.0, 0.0, 0.0, 0.0],
            "limit": 5,
        }),
    )
    .await;
    assert!(found.ok, "search reply: {found:?}");
    assert_eq!(found.data["status"], "ok");
    let results = found.data["results"].as_array().expect("results array");
    assert!(!results.is_empty(), "added row must surface in search");
    assert_eq!(results[0]["id"], id);
    assert_eq!(results[0]["memory_type"], "episodic");

    server.stop();
    let _ = tokio::time::timeout(Duration::from_secs(2), task).await;

    match prior {
        Some(v) => std::env::set_var("WYLDE_DATA_DIR", v),
        None => std::env::remove_var("WYLDE_DATA_DIR"),
    }
    match prior_dim {
        Some(v) => std::env::set_var("WYLDE_EMBED_DIM", v),
        None => std::env::remove_var("WYLDE_EMBED_DIM"),
    }
}

/// Restore (or clear) an env var to a previously-captured value.
fn restore(key: &str, prior: Option<std::ffi::OsString>) {
    match prior {
        Some(v) => std::env::set_var(key, v),
        None => std::env::remove_var(key),
    }
}
