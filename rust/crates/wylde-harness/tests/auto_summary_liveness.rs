//! M1 liveness — the auto-summary **producer** is actually wired.
//!
//! The whole summary pipeline (cadence, parse, persist, tier-2 render)
//! was unit-tested since B2, but `needs_regen` / `refresh_standalone`
//! had zero callers — the classic dead-producer defect this plan's M8
//! net exists for. These tests drive the *wiring*:
//!
//! * the producer seam itself ([`maybe_refresh`]) attaches the derived
//!   fields to a standalone conversation through a mock `wylde-ollama`,
//! * the `WYLDE_AUTO_SUMMARY` kill switch really kills it,
//! * the post-turn hook fires it end-to-end from `chat.run_turn`.
//!
//! Same shared-mock-pipe pattern as `run_turn_loop_e2e.rs` (`Config`
//! freezes the service name process-wide; tests serialise on a mutex).
//!
//! Windows-only — IPC uses named pipes.

#![cfg(windows)]

use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Map, Value};
use tokio::sync::{Mutex as AsyncMutex, MutexGuard, OnceCell};
use wylde_harness::chat::search::summary;
use wylde_harness::memory::conversations::store as conv_store;
use wylde_harness::turn::actions as chat;
use wylde_shared::ipc;

struct Harness {
    _server: Arc<ipc::PipeServer>,
}

async fn harness() -> &'static Harness {
    static H: OnceCell<Harness> = OnceCell::const_new();
    H.get_or_init(|| async {
        let suffix = uuid::Uuid::new_v4().simple().to_string();
        let service = format!("ollama-mock-{suffix}");
        std::env::set_var("WYLDE_HARNESS_OLLAMA_SERVICE", &service);
        std::env::set_var("WYLDE_DEFAULT_MODEL", "stub-default");
        // 4-dim embeddings keep the mock vectors readable.
        std::env::set_var("WYLDE_EMBED_NATIVE_DIM", "4");
        std::env::set_var("WYLDE_EMBED_DIM", "4");

        ipc::register_action("ollama.chat", move |_payload: Value| async move {
            ipc::Reply::ok(json!({
                "message": {"content": "A neat summary of the chat.\nTags: alpha, beta"}
            }))
        });
        // One unit vector per requested input (the embedder validates
        // count + native dim).
        ipc::register_action("ollama.embed", move |payload: Value| async move {
            let n = payload
                .get("input")
                .and_then(Value::as_array)
                .map(|a| a.len())
                .unwrap_or(1);
            let vecs: Vec<Value> = (0..n).map(|_| json!([0.5, 0.5, 0.5, 0.5])).collect();
            ipc::Reply::ok(json!({ "embeddings": vecs }))
        });

        let server = Arc::new(ipc::PipeServer::new(&service));
        let server_clone = Arc::clone(&server);
        // Dedicated OS thread + runtime so the server outlives each
        // `#[tokio::test]` runtime (same rationale as the e2e binary).
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("server runtime");
            let _ = rt.block_on(server_clone.accept_loop());
        });
        tokio::time::sleep(Duration::from_millis(300)).await;

        Harness { _server: server }
    })
    .await
}

/// Serialise tests (shared mock pipe + process-wide env) and pin
/// `WYLDE_DATA_DIR` to a fresh tempdir for the body.
async fn test_guard<'a>() -> (MutexGuard<'a, ()>, tempfile::TempDir) {
    static LOCK: AsyncMutex<()> = AsyncMutex::const_new(());
    let guard = LOCK.lock().await;
    harness().await;
    let tempdir = tempfile::TempDir::new().expect("tempdir");
    std::env::set_var("WYLDE_DATA_DIR", tempdir.path());
    (guard, tempdir)
}

/// Seed a standalone conversation with `n` persisted messages.
fn seed_conversation(id: &str, n: usize) {
    let mut doc = Map::new();
    doc.insert("id".to_owned(), json!(id));
    doc.insert("title".to_owned(), json!("Seeded"));
    doc.insert(
        "messages".to_owned(),
        Value::Array(
            (0..n)
                .map(|i| {
                    json!({
                        "role": if i % 2 == 0 { "user" } else { "assistant" },
                        "content": format!("message {i}"),
                    })
                })
                .collect(),
        ),
    );
    conv_store::save_conversation(&doc).expect("seed conversation");
}

#[tokio::test]
async fn producer_attaches_summary_fields_to_standalone_conversation() {
    let (_g, _dir) = test_guard().await;
    seed_conversation("as1", 5);

    summary::maybe_refresh("as1", None).await;

    let doc = conv_store::read_conversation("as1").expect("doc");
    assert_eq!(doc["auto_summary"], "A neat summary of the chat.");
    assert_eq!(doc["topic_tags"], json!(["alpha", "beta"]));
    assert_eq!(doc["embedding"].as_array().expect("embedding").len(), 4);
    assert_eq!(doc["summary_msg_count"], 5);
    // The freshly stamped bucket makes the next call a no-op.
    assert!(!summary::needs_regen(&doc));
}

/// C8 write-side complement to D2. A workspace-*bound* conversation is a
/// flat doc carrying `workspace_id` (Route 1). Its auto-summary must land
/// on that same flat doc — the tier-2 gather slot reads it back from the
/// flat store regardless of binding — and never the legacy per-workspace
/// service store. Before C8, `maybe_refresh` routed a bound summary to the
/// service store, leaving the bound conversation's tier-2 slot permanently
/// empty and diverging from conversation reflection (which already writes
/// bound output harness-side, prompt-visible, never long-term / never a
/// second live store). This pins the reconciliation: bound → flat store.
#[tokio::test]
async fn producer_writes_bound_conversation_summary_to_flat_store() {
    let (_g, _dir) = test_guard().await;
    seed_conversation("as-bound", 6);
    conv_store::set_workspace("as-bound", Some("ws-bound")).expect("bind to workspace");

    summary::maybe_refresh("as-bound", Some("ws-bound")).await;

    let doc = conv_store::read_conversation("as-bound").expect("flat doc");
    assert_eq!(
        doc["auto_summary"], "A neat summary of the chat.",
        "a bound conversation's summary must land on its flat doc, not the service store"
    );
    assert_eq!(doc["topic_tags"], json!(["alpha", "beta"]));
    assert_eq!(doc["summary_msg_count"], 6);
    assert_eq!(doc["embedding"].as_array().expect("embedding").len(), 4);
    // The binding survives the summary merge (it is a field on the same doc).
    assert_eq!(doc["workspace_id"], "ws-bound");
    // Freshly stamped bucket → next call is a no-op.
    assert!(!summary::needs_regen(&doc));
}

#[tokio::test]
async fn kill_switch_disables_producer() {
    let (_g, _dir) = test_guard().await;
    seed_conversation("as2", 5);

    std::env::set_var("WYLDE_AUTO_SUMMARY", "off");
    summary::maybe_refresh("as2", None).await;
    std::env::remove_var("WYLDE_AUTO_SUMMARY");

    let doc = conv_store::read_conversation("as2").expect("doc");
    assert!(
        doc.get("auto_summary").is_none(),
        "kill switch must prevent the summary write, got {doc:?}"
    );
}

/// The summary counted here is **7**, not the 5 seeded (#242).
///
/// The turn path now persists its own exchange before the summary hook
/// runs (`run_post_turn_hooks` → `conversations::store::append_exchange`),
/// so the document the summariser reads is the 5 seeded messages plus this
/// turn's user message and reply. That ordering is deliberate: a summary
/// that omitted the turn which triggered it would always be one exchange
/// stale. The old `5` was only ever right because nothing on the turn path
/// wrote to `messages` at all — the bug this asserts the absence of.
#[tokio::test]
async fn post_turn_hook_drives_producer_end_to_end() {
    let (_g, _dir) = test_guard().await;
    seed_conversation("as3", 5);

    let reply = chat::handle_run_turn(json!({
        "user_message": "hi there",
        "conversation_id": "as3",
        "model": "stub",
    }))
    .await;
    assert!(reply.ok, "turn should complete, got {reply:?}");

    // The hook is spawned fire-and-forget; poll for the persisted field.
    let mut found = false;
    for _ in 0..100 {
        let doc = conv_store::read_conversation("as3").expect("doc");
        if doc
            .get("auto_summary")
            .and_then(Value::as_str)
            .is_some_and(|s| !s.is_empty())
        {
            assert_eq!(
                doc["summary_msg_count"], 7,
                "5 seeded + the turn's own user message and reply — the \
                 summariser must see the exchange that triggered it (#242)"
            );
            // And the exchange really is the turn's, not filler.
            let msgs = doc["messages"].as_array().expect("messages");
            assert_eq!(msgs.len(), 7);
            assert_eq!(msgs[5]["role"], "user");
            assert_eq!(msgs[5]["content"], "hi there");
            assert_eq!(msgs[6]["role"], "assistant");
            assert_eq!(doc["embedding"].as_array().expect("embedding").len(), 4);
            found = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(
        found,
        "post-turn hook never attached auto_summary within the poll window"
    );
}
