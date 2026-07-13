//! `tools.*` unit tests for the `HarnessApi` surface, split out of the
//! `api.rs` tests mod per architecture-review R1.

use serde_json::{json, Value};

use crate::api::{DefaultHarnessApi, HarnessApi};
use crate::memory::long_term::test_support::TestEnv;

// ── tools.* unit tests (moved from pipe/tools.rs) ────────────────

#[tokio::test]
async fn tools_list_returns_catalog_with_count() {
    let api = DefaultHarnessApi;
    let reply = api.tools_list(Value::Null).await;
    assert!(reply.ok);
    let tools = reply.data["tools"].as_array().expect("array");
    let count = reply.data["count"].as_u64().expect("count is uint");
    assert_eq!(count as usize, tools.len());
    let ids: Vec<&str> = tools.iter().filter_map(|t| t["id"].as_str()).collect();
    assert!(
        ids.contains(&"time_now"),
        "expected `time_now` in catalog, got {ids:?}"
    );
}

#[tokio::test]
async fn tools_list_entries_carry_status_and_destructive_flag() {
    let api = DefaultHarnessApi;
    let reply = api.tools_list(Value::Null).await;
    assert!(reply.ok);
    let first = &reply.data["tools"][0];
    assert!(first["status"].is_string(), "status must be a string");
    assert!(
        first["destructive"].is_boolean(),
        "destructive must be a bool"
    );
}

#[tokio::test]
async fn tools_run_rejects_missing_name() {
    let api = DefaultHarnessApi;
    let reply = api.tools_run(json!({})).await;
    assert!(!reply.ok);
    assert_eq!(reply.error.unwrap().code, "bad_request");
}

#[tokio::test]
async fn tools_run_rejects_non_map_args() {
    let api = DefaultHarnessApi;
    let reply = api
        .tools_run(json!({"name": "time.now", "args": "oops"}))
        .await;
    assert!(!reply.ok);
    assert_eq!(reply.error.unwrap().code, "bad_request");
}

#[tokio::test]
async fn tools_run_dispatches_active_tool_and_returns_ok_envelope() {
    // Phase 12.2 consent gate guards every dispatch; bypass it
    // here under the shared serial guard so the existing
    // tools.run semantics keep being pinned. New consent
    // integration tests live in `tooling::runner::tests`.
    let _g = crate::tooling::consent::bypass_scope(true).await;
    let api = DefaultHarnessApi;
    let reply = api.tools_run(json!({"name": "time.now"})).await;
    assert!(reply.ok, "outer envelope is ok");
    assert_eq!(reply.data["ok"], true);
    assert_eq!(reply.data["canonical_id"], "time_now");
    assert_eq!(reply.data["data"]["status"], "success");
}

#[tokio::test]
async fn tools_run_returns_not_found_for_unknown_tool() {
    let api = DefaultHarnessApi;
    let reply = api
        .tools_run(json!({"name": "definitely.not.a.tool"}))
        .await;
    assert!(reply.ok, "outer envelope is ok (transport-level)");
    assert_eq!(reply.data["ok"], false);
    assert_eq!(reply.data["error"]["code"], "not_found");
}

// ── Tool-registry consolidation Slice 1/2 — verb-tool smoke tests ──
//
// The eight verb tools co-exist with the old named tools in the
// catalog. These prove the full `tools.run` → runner → tier gate →
// consent gate → verb handler → ResourceRegistry path returns valid
// JSON. Slice 2 lights up the `memory` resource, so describe now
// surfaces it through the full pipeline.

#[tokio::test]
async fn tools_run_dispatches_wylde_describe_through_full_pipeline() {
    let _g = crate::tooling::consent::bypass_scope(true).await;
    let api = DefaultHarnessApi;
    let reply = api.tools_run(json!({"name": "wylde_describe"})).await;
    assert!(reply.ok, "outer envelope is ok");
    assert_eq!(reply.data["ok"], true);
    assert_eq!(reply.data["canonical_id"], "wylde_describe");
    assert_eq!(reply.data["data"]["status"], "success");
    // Slice 2: the `memory` resource is registered, so describe lists it.
    let rows = reply.data["data"]["resources"].as_array().unwrap();
    assert!(
        rows.iter().any(|r| r["resource_type"] == "memory"),
        "describe should surface the memory resource through the full pipeline",
    );
    assert_eq!(
        reply.data["data"]["count"].as_u64().unwrap(),
        rows.len() as u64
    );
}

#[tokio::test]
async fn tools_run_wylde_list_unknown_resource_is_clean_not_found() {
    let _g = crate::tooling::consent::bypass_scope(true).await;
    let api = DefaultHarnessApi;
    let reply = api
        .tools_run(json!({"name": "wylde_list", "args": {"resource_type": "nope"}}))
        .await;
    assert!(reply.ok);
    // Transport + dispatch succeed; the verb returns a structured
    // not-found envelope (not a hard error) so the model can recover.
    assert_eq!(reply.data["ok"], true);
    assert_eq!(reply.data["data"]["status"], "not_found");
    assert_eq!(reply.data["data"]["op"], "list");
}

#[tokio::test]
async fn tools_run_wylde_create_then_get_memory_full_pipeline() {
    // Slice 2: the memory verb path end-to-end through the runner —
    // `wylde_create` (destructive, consent-gated) then `wylde_get`.
    let _g = crate::tooling::consent::bypass_scope(true).await;
    let _env = TestEnv::new();
    std::env::set_var("WYLDE_EMBED_DIM", "3");
    let api = DefaultHarnessApi;

    // create is destructive → needs the destructive tier (consent is
    // bypassed above; the tier gate is separate).
    let created = api
            .tools_run(json!({
                "name": "wylde_create",
                "device_tier": "destructive_tool_access",
                "args": {"resource_type": "memory", "body": {"body": "pipeline memory", "importance": 7}},
            }))
            .await;
    assert!(created.ok);
    assert_eq!(created.data["ok"], true);
    assert_eq!(created.data["canonical_id"], "wylde_create");
    assert_eq!(created.data["data"]["status"], "success");
    let id = created.data["data"]["id"].as_str().unwrap().to_owned();

    let got = api
        .tools_run(json!({
            "name": "wylde_get",
            "args": {"resource_type": "memory", "resource_id": id},
        }))
        .await;
    assert!(got.ok);
    assert_eq!(got.data["data"]["status"], "success");
    assert_eq!(got.data["data"]["memory"]["body"], "pipeline memory");
}
