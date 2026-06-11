//! `rag.*` trait-method unit tests (Wylde_Study S2a), split out of the
//! `api.rs` tests mod per architecture-review R1.

use serde_json::json;

use crate::api::{DefaultHarnessApi, HarnessApi};
use crate::memory::long_term::test_support::TestEnv;

// ── rag.* trait-method tests (Wylde_Study S2a) ──────────────────
// Exercise the api-layer wrappers: validation surfaces as a
// `status`-envelope inside an ok Reply, and an add → search
// round-trip works through the trait with precomputed vectors (no
// live wylde-ollama needed).

fn set_embed_dim_4() {
    std::env::set_var("WYLDE_EMBED_DIM", "4");
}

#[tokio::test]
async fn rag_add_episodic_rejects_missing_content() {
    let _env = TestEnv::new();
    set_embed_dim_4();
    let api = DefaultHarnessApi;
    let reply = api.rag_add_episodic(json!({"url": "http://x"})).await;
    assert!(reply.ok, "transport-level ok");
    assert_eq!(reply.data["status"], "error");
}

#[tokio::test]
async fn rag_search_rejects_missing_q() {
    let _env = TestEnv::new();
    set_embed_dim_4();
    let api = DefaultHarnessApi;
    let reply = api
        .rag_search(json!({"query_vector": [1.0, 0.0, 0.0, 0.0]}))
        .await;
    assert!(reply.ok);
    assert_eq!(reply.data["status"], "error");
}

#[tokio::test]
async fn rag_add_episodic_then_search_round_trips_via_trait() {
    let _env = TestEnv::new();
    set_embed_dim_4();
    let api = DefaultHarnessApi;

    let added = api
        .rag_add_episodic(json!({
            "content": "trait-path episodic body",
            "url": "http://x/page",
            "vector": [1.0, 0.0, 0.0, 0.0],
        }))
        .await;
    assert!(added.ok);
    assert_eq!(added.data["status"], "ok");
    let id = added.data["memory_id"].as_str().unwrap().to_owned();

    let found = api
        .rag_search(json!({
            "q": "trait body",
            "query_vector": [1.0, 0.0, 0.0, 0.0],
        }))
        .await;
    assert!(found.ok);
    assert_eq!(found.data["status"], "ok");
    let results = found.data["results"].as_array().unwrap();
    assert!(!results.is_empty());
    assert_eq!(results[0]["id"], id);
}
