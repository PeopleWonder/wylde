//! `memory.long_term.*` unit tests for the `HarnessApi` surface, split
//! out of the `api.rs` tests mod per architecture-review R1.

use serde_json::{json, Value};

use crate::api::{DefaultHarnessApi, HarnessApi};
use crate::memory::long_term::test_support::TestEnv;

// ── memory.long_term.* unit tests (moved from pipe/memory_long_term.rs) ──

#[tokio::test]
async fn long_term_list_empty_returns_zero_count() {
    let _env = TestEnv::new();
    let api = DefaultHarnessApi;
    let reply = api.memory_long_term_list(Value::Null).await;
    assert!(reply.ok);
    assert_eq!(reply.data["count"], 0);
    assert_eq!(reply.data["memories"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn long_term_save_rejects_missing_body() {
    let _env = TestEnv::new();
    let api = DefaultHarnessApi;
    let reply = api.memory_long_term_save(json!({})).await;
    assert!(!reply.ok);
    assert_eq!(reply.error.unwrap().code, "bad_request");
}

#[tokio::test]
async fn long_term_save_rejects_blank_body() {
    let _env = TestEnv::new();
    let api = DefaultHarnessApi;
    let reply = api.memory_long_term_save(json!({"body": ""})).await;
    assert!(!reply.ok);
    assert_eq!(reply.error.unwrap().code, "bad_request");
}

#[tokio::test]
async fn long_term_save_persists_record_and_returns_dict() {
    let _env = TestEnv::new();
    let api = DefaultHarnessApi;
    let reply = api
        .memory_long_term_save(json!({
            "body": "remember the alamo",
            "source": "settings_ui",
            "importance": 7,
            "tags": ["history", "tx"],
        }))
        .await;
    assert!(reply.ok, "save should succeed: {reply:?}");
    assert_eq!(reply.data["body"], "remember the alamo");
    assert_eq!(reply.data["source"], "settings_ui");
    assert_eq!(reply.data["importance"], 7);
    let tags = reply.data["tags"].as_array().unwrap();
    assert_eq!(tags.len(), 2);

    let list = api.memory_long_term_list(Value::Null).await;
    assert_eq!(list.data["count"], 1);
}

#[tokio::test]
async fn long_term_save_mirrors_caller_supplied_vector(/* regression: fix #43 */) {
    // Before the fix the API/pipe save handler passed `None` for the
    // vector — even a caller-supplied embedding was dropped, so the record
    // never entered `long_term.vec.bin` and vector search couldn't rank it.
    // Now the handler threads the caller vector through. Deterministic:
    // a unit basis vector at the default embed dim, no live embedder.
    let _env = TestEnv::new();
    let api = DefaultHarnessApi;
    let dim = crate::memory::common::embed_dim();
    let mut v = vec![0.0f32; dim];
    v[0] = 1.0;

    let reply = api
        .memory_long_term_save(json!({ "body": "the mirrored memory", "vector": v.clone() }))
        .await;
    assert!(reply.ok, "save failed: {reply:?}");
    let id = reply.data["id"].as_str().unwrap().to_owned();

    // Store-level vector search with the SAME vector: a mirrored vector
    // ranks at similarity ~1.0; a dropped one wouldn't appear at all.
    let hits = crate::memory::long_term::search(v, 5, None);
    let hit = hits
        .iter()
        .find(|h| h.id == id)
        .expect("caller vector was not mirrored — vector search found nothing");
    assert!(
        hit.similarity > 0.99,
        "expected self-similarity ~1.0, got {}",
        hit.similarity
    );
}

#[tokio::test]
async fn long_term_save_accepts_float_importance() {
    let _env = TestEnv::new();
    let api = DefaultHarnessApi;
    let reply = api
        .memory_long_term_save(json!({
            "body": "float importance",
            "importance": 6.5_f64,
        }))
        .await;
    assert!(reply.ok);
    let imp = reply.data["importance"].as_i64().expect("integer");
    assert!((1..=10).contains(&imp), "importance out of range: {imp}");
}

#[tokio::test]
async fn long_term_update_returns_not_found_for_unknown_id() {
    let _env = TestEnv::new();
    let api = DefaultHarnessApi;
    let reply = api
        .memory_long_term_update(json!({"id": "deadbeef", "body": "x"}))
        .await;
    assert!(!reply.ok);
    assert_eq!(reply.error.unwrap().code, "not_found");
}

#[tokio::test]
async fn long_term_update_rejects_missing_id() {
    let _env = TestEnv::new();
    let api = DefaultHarnessApi;
    let reply = api.memory_long_term_update(json!({})).await;
    assert!(!reply.ok);
    assert_eq!(reply.error.unwrap().code, "bad_request");
}

#[tokio::test]
async fn long_term_update_supersedes_existing_record() {
    let _env = TestEnv::new();
    let api = DefaultHarnessApi;
    let saved = api.memory_long_term_save(json!({"body": "v1"})).await;
    let id = saved.data["id"].as_str().unwrap().to_owned();

    let updated = api
        .memory_long_term_update(json!({"id": id, "body": "v2"}))
        .await;
    assert!(updated.ok);
    assert_eq!(updated.data["body"], "v2");
    let new_id = updated.data["id"].as_str().unwrap();
    assert_ne!(new_id, id, "update must mint a new record id");
}

#[tokio::test]
async fn long_term_delete_returns_ok_false_for_unknown_id() {
    let _env = TestEnv::new();
    let api = DefaultHarnessApi;
    let reply = api.memory_long_term_delete(json!({"id": "deadbeef"})).await;
    assert!(reply.ok);
    assert_eq!(reply.data["ok"], false);
}

#[tokio::test]
async fn long_term_delete_removes_existing_record() {
    let _env = TestEnv::new();
    let api = DefaultHarnessApi;
    let saved = api
        .memory_long_term_save(json!({"body": "to delete"}))
        .await;
    let id = saved.data["id"].as_str().unwrap().to_owned();
    let del = api.memory_long_term_delete(json!({"id": id.clone()})).await;
    assert_eq!(del.data["ok"], true);
    assert_eq!(del.data["id"], id);

    let list = api.memory_long_term_list(Value::Null).await;
    assert_eq!(list.data["count"], 0);
}

#[tokio::test]
async fn long_term_delete_rejects_missing_id() {
    let _env = TestEnv::new();
    let api = DefaultHarnessApi;
    let reply = api.memory_long_term_delete(json!({})).await;
    assert!(!reply.ok);
    assert_eq!(reply.error.unwrap().code, "bad_request");
}

#[tokio::test]
async fn long_term_history_rejects_missing_id() {
    let _env = TestEnv::new();
    let api = DefaultHarnessApi;
    let reply = api.memory_long_term_history(json!({})).await;
    assert!(!reply.ok);
    assert_eq!(reply.error.unwrap().code, "bad_request");
}

#[tokio::test]
async fn long_term_history_returns_empty_chain_for_unknown_id() {
    let _env = TestEnv::new();
    let api = DefaultHarnessApi;
    let reply = api
        .memory_long_term_history(json!({"id": "deadbeef"}))
        .await;
    assert!(reply.ok);
    assert_eq!(reply.data["id"], "deadbeef");
    assert_eq!(reply.data["chain"].as_array().unwrap().len(), 0);
}

#[tokio::test]
async fn long_term_history_walks_chain_after_update() {
    let _env = TestEnv::new();
    let api = DefaultHarnessApi;
    let saved = api.memory_long_term_save(json!({"body": "v1"})).await;
    let v1_id = saved.data["id"].as_str().unwrap().to_owned();
    let updated = api
        .memory_long_term_update(json!({"id": v1_id, "body": "v2"}))
        .await;
    let v2_id = updated.data["id"].as_str().unwrap().to_owned();

    let reply = api
        .memory_long_term_history(json!({"id": v2_id.clone()}))
        .await;
    assert!(reply.ok);
    let chain = reply.data["chain"].as_array().unwrap();
    assert_eq!(chain.len(), 2);
    let bodies: Vec<&str> = chain.iter().map(|v| v["body"].as_str().unwrap()).collect();
    assert_eq!(bodies, vec!["v1", "v2"]);
}

#[tokio::test]
async fn long_term_list_excludes_superseded_by_default() {
    let _env = TestEnv::new();
    let api = DefaultHarnessApi;
    let saved = api.memory_long_term_save(json!({"body": "v1"})).await;
    let v1_id = saved.data["id"].as_str().unwrap().to_owned();
    let _ = api
        .memory_long_term_update(json!({"id": v1_id, "body": "v2"}))
        .await;

    let default_list = api.memory_long_term_list(Value::Null).await;
    assert_eq!(default_list.data["count"], 1);

    let with_superseded = api
        .memory_long_term_list(json!({"include_superseded": true}))
        .await;
    assert_eq!(with_superseded.data["count"], 2);
}
