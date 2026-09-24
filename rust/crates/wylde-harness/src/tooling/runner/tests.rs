use super::*;
use crate::tooling::consent::{self, Decision};
use crate::tooling::registry::{entry_active, entry_deferred};
use serde_json::json;

/// Acquire the consent serial guard and pin bypass=true for the
/// scope's lifetime (restored to the default `false` on drop, so
/// the flag never leaks into whichever test runs next). The two
/// dispatch-test families (bypass-needed and gate-needed) share
/// the same guard so a gate test never overlaps with a
/// bypass-needed test mid-flight.
async fn bypass_scope() -> consent::BypassScope {
    consent::bypass_scope(true).await
}

fn make_active_read_only_entry() -> ToolEntry {
    entry_active(
        "read_file",
        "fs.read_file",
        "fs",
        "read a file",
        vec![],
        false,
        |args, _| async move { Ok(json!({"echo": args})) },
    )
}

fn make_active_destructive_entry() -> ToolEntry {
    entry_active(
        "write_file",
        "fs.write_file",
        "fs",
        "write a file",
        vec![],
        true,
        |args, _| async move { Ok(json!({"wrote": args})) },
    )
}

fn make_deferred_entry() -> ToolEntry {
    entry_deferred(
        "memory_search",
        "memory.search",
        "memory",
        "memory search",
        vec![],
        false,
        "7",
        "lands with memory port",
    )
}

#[tokio::test]
async fn dispatch_returns_not_found_for_unknown_tool() {
    let _g = bypass_scope().await;
    let cfg = Config::default_for_tests();
    let cfg: &'static Config = Box::leak(Box::new(cfg));
    let reg = Registry::with_only(vec![]);
    let outcome = dispatch_tool(&reg, cfg, "no.such.tool", TIER_TOOL_USE, json!({}), false).await;
    let err = outcome.result.expect_err("should fail");
    assert_eq!(err.error.code, "not_found");
    assert_eq!(err.reason, Some(ToolErrorReason::ToolCallTextUnrecognised));
}

#[tokio::test]
async fn dispatch_invokes_active_handler() {
    let _g = bypass_scope().await;
    let cfg = Config::default_for_tests();
    let cfg: &'static Config = Box::leak(Box::new(cfg));
    let reg = Registry::with_only(vec![make_active_read_only_entry()]);
    let outcome = dispatch_tool(
        &reg,
        cfg,
        "fs.read_file",
        TIER_TOOL_USE,
        json!({"path": "x"}),
        false,
    )
    .await;
    let ok = outcome.result.expect("active handler succeeds");
    assert_eq!(ok["echo"]["path"], "x");
    assert_eq!(outcome.canonical_id, "read_file");
}

#[tokio::test]
async fn dispatch_returns_deferred_error_for_deferred_entry() {
    let _g = bypass_scope().await;
    let cfg = Config::default_for_tests();
    let cfg: &'static Config = Box::leak(Box::new(cfg));
    let reg = Registry::with_only(vec![make_deferred_entry()]);
    let outcome = dispatch_tool(&reg, cfg, "memory_search", TIER_TOOL_USE, json!({}), false).await;
    let err = outcome.result.expect_err("should fail");
    assert_eq!(err.error.code, "phase_7_deferred");
    assert!(err.error.message.contains("Phase 7"));
}

#[tokio::test]
async fn dispatch_blocks_read_only_tier_for_every_tool() {
    let _g = bypass_scope().await;
    let cfg = Config::default_for_tests();
    let cfg: &'static Config = Box::leak(Box::new(cfg));
    let reg = Registry::with_only(vec![make_active_read_only_entry()]);
    let outcome = dispatch_tool(&reg, cfg, "fs.read_file", TIER_READ_ONLY, json!({}), false).await;
    let err = outcome.result.expect_err("should block");
    assert_eq!(err.reason, Some(ToolErrorReason::TierReadOnly));
    assert_eq!(err.error.code, "tier_read_only");
}

#[tokio::test]
async fn dispatch_blocks_destructive_tool_on_tool_use_tier() {
    let _g = bypass_scope().await;
    let cfg = Config::default_for_tests();
    let cfg: &'static Config = Box::leak(Box::new(cfg));
    let reg = Registry::with_only(vec![make_active_destructive_entry()]);
    let outcome = dispatch_tool(&reg, cfg, "fs.write_file", TIER_TOOL_USE, json!({}), false).await;
    let err = outcome.result.expect_err("should block");
    assert_eq!(err.reason, Some(ToolErrorReason::TierReadOnly));
    assert_eq!(err.error.code, "tier_tool_use_destructive_blocked");
}

#[tokio::test]
async fn dispatch_allows_destructive_tool_on_destructive_tier() {
    let _g = bypass_scope().await;
    let cfg = Config::default_for_tests();
    let cfg: &'static Config = Box::leak(Box::new(cfg));
    let reg = Registry::with_only(vec![make_active_destructive_entry()]);
    let outcome = dispatch_tool(
        &reg,
        cfg,
        "fs.write_file",
        TIER_DESTRUCTIVE,
        json!({"a": 1}),
        false,
    )
    .await;
    let ok = outcome.result.expect("destructive tier permits");
    assert_eq!(ok["wrote"]["a"], 1);
}

// ── Phase 12.2 consent-gate integration tests ────────────────────
//
// These run under a shared serial guard with bypass=off so the
// global consent store is in a known shape during the dispatch.
// The store's persistence path is redirected to a per-test
// tempdir so the host's real `data/preferences/consent.json` is
// never touched.

async fn gate_test_scope<F, Fut>(test_body: F)
where
    F: FnOnce(tempfile::TempDir) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let _guard = consent::bypass_scope(false).await;
    let td = tempfile::TempDir::new().expect("tempdir");
    let path = td.path().join("consent.json");
    // Inject a fresh store-of-record so the test's writes don't
    // race with the real file at <wylde_root>/data/preferences/.
    consent_store_set_path_for_tests(path);
    consent::store().reset().expect("reset injected store");
    // Phase 12.6: hitting the gate now also records a pending
    // entry in the process-wide registry. Clear it so the
    // previous test's residue doesn't bleed in.
    consent::clear_pending();
    test_body(td).await;
}

fn consent_store_set_path_for_tests(path: std::path::PathBuf) {
    // Tunnel through the consent module's test-only helper. Kept
    // separate so the public API surface stays minimal.
    let s = consent::store();
    consent::store_set_path_for_tests_helper(s, path);
}

#[tokio::test]
async fn dispatch_returns_consent_required_when_no_decision() {
    gate_test_scope(|_td| async {
        let cfg = Config::default_for_tests();
        let cfg: &'static Config = Box::leak(Box::new(cfg));
        let reg = Registry::with_only(vec![make_active_read_only_entry()]);
        let outcome =
            dispatch_tool(&reg, cfg, "fs.read_file", TIER_TOOL_USE, json!({}), false).await;
        let err = outcome.result.expect_err("gate should block");
        assert_eq!(err.error.code, "consent_required");
        assert_eq!(err.reason, Some(ToolErrorReason::ConsentRequired));
        let details = err.error.details.as_ref().expect("details present");
        assert_eq!(details["tool_id"], "read_file");
        assert!(details["prompt"].as_str().unwrap().contains("fs.read_file"));
    })
    .await;
}

#[tokio::test]
async fn dispatch_passes_after_approved_decision() {
    gate_test_scope(|_td| async {
        consent::store()
            .set("read_file", Decision::Approved)
            .expect("set approved");
        let cfg = Config::default_for_tests();
        let cfg: &'static Config = Box::leak(Box::new(cfg));
        let reg = Registry::with_only(vec![make_active_read_only_entry()]);
        let outcome = dispatch_tool(
            &reg,
            cfg,
            "fs.read_file",
            TIER_TOOL_USE,
            json!({"path": "x"}),
            false,
        )
        .await;
        let ok = outcome.result.expect("approved tool dispatches");
        assert_eq!(ok["echo"]["path"], "x");
    })
    .await;
}

#[tokio::test]
async fn dispatch_blocks_with_consent_denied_when_denied() {
    gate_test_scope(|_td| async {
        consent::store()
            .set("read_file", Decision::Denied)
            .expect("set denied");
        let cfg = Config::default_for_tests();
        let cfg: &'static Config = Box::leak(Box::new(cfg));
        let reg = Registry::with_only(vec![make_active_read_only_entry()]);
        let outcome =
            dispatch_tool(&reg, cfg, "fs.read_file", TIER_TOOL_USE, json!({}), false).await;
        let err = outcome.result.expect_err("denied gate blocks");
        assert_eq!(err.error.code, "consent_denied");
        assert_eq!(err.reason, Some(ToolErrorReason::ConsentDenied));
    })
    .await;
}

#[tokio::test]
async fn dispatch_confirm_satisfies_undecided_gate() {
    // Per-call confirm on an undecided destructive tool (with the
    // destructive tier) executes it end-to-end — no GUI consent step.
    gate_test_scope(|_td| async {
        let cfg = Config::default_for_tests();
        let cfg: &'static Config = Box::leak(Box::new(cfg));
        let reg = Registry::with_only(vec![make_active_destructive_entry()]);
        let outcome = dispatch_tool(
            &reg,
            cfg,
            "fs.write_file",
            TIER_DESTRUCTIVE,
            json!({"a": 1}),
            true,
        )
        .await;
        let ok = outcome.result.expect("confirm clears the undecided gate");
        assert_eq!(ok["wrote"]["a"], 1);
    })
    .await;
}

#[tokio::test]
async fn dispatch_confirm_never_overrides_a_stored_deny() {
    // The critical guardrail: a stored explicit deny wins over a
    // per-call confirm. A remote/MCP caller can never override the
    // user's local "deny".
    gate_test_scope(|_td| async {
        consent::store()
            .set("write_file", Decision::Denied)
            .expect("set denied");
        let cfg = Config::default_for_tests();
        let cfg: &'static Config = Box::leak(Box::new(cfg));
        let reg = Registry::with_only(vec![make_active_destructive_entry()]);
        let outcome = dispatch_tool(
            &reg,
            cfg,
            "fs.write_file",
            TIER_DESTRUCTIVE,
            json!({"a": 1}),
            true,
        )
        .await;
        let err = outcome.result.expect_err("deny must win over confirm");
        assert_eq!(err.error.code, "consent_denied");
        assert_eq!(err.reason, Some(ToolErrorReason::ConsentDenied));
    })
    .await;
}

#[tokio::test]
async fn dispatch_confirm_is_per_call_not_persisted() {
    // A confirmed call does not record a standing decision: the next
    // call WITHOUT confirm hits the undecided gate again.
    gate_test_scope(|_td| async {
        let cfg = Config::default_for_tests();
        let cfg: &'static Config = Box::leak(Box::new(cfg));
        let reg = Registry::with_only(vec![make_active_destructive_entry()]);
        // First call confirms and runs.
        let ok = dispatch_tool(
            &reg,
            cfg,
            "fs.write_file",
            TIER_DESTRUCTIVE,
            json!({"a": 1}),
            true,
        )
        .await;
        assert!(ok.result.is_ok(), "confirmed call runs");
        // Second call, no confirm → gate is still undecided.
        let outcome = dispatch_tool(
            &reg,
            cfg,
            "fs.write_file",
            TIER_DESTRUCTIVE,
            json!({"a": 2}),
            false,
        )
        .await;
        let err = outcome.result.expect_err("confirm was not persisted");
        assert_eq!(err.error.code, "consent_required");
    })
    .await;
}

#[tokio::test]
async fn dispatch_confirm_does_not_bypass_the_tier_gate() {
    // Confirm satisfies consent, never the tier gate: a destructive
    // tool on the tool_use tier is still blocked even with confirm.
    gate_test_scope(|_td| async {
        let cfg = Config::default_for_tests();
        let cfg: &'static Config = Box::leak(Box::new(cfg));
        let reg = Registry::with_only(vec![make_active_destructive_entry()]);
        let outcome = dispatch_tool(
            &reg,
            cfg,
            "fs.write_file",
            TIER_TOOL_USE,
            json!({"a": 1}),
            true,
        )
        .await;
        let err = outcome.result.expect_err("tier blocks before consent");
        assert_eq!(err.error.code, "tier_tool_use_destructive_blocked");
    })
    .await;
}

#[tokio::test]
async fn dispatch_passes_when_global_no_auth_set() {
    gate_test_scope(|_td| async {
        consent::store().set_no_auth(true).expect("set no_auth");
        let cfg = Config::default_for_tests();
        let cfg: &'static Config = Box::leak(Box::new(cfg));
        let reg = Registry::with_only(vec![make_active_read_only_entry()]);
        let outcome =
            dispatch_tool(&reg, cfg, "fs.read_file", TIER_TOOL_USE, json!({}), false).await;
        let ok = outcome.result.expect("no_auth skips the gate");
        assert_eq!(ok["echo"], json!({}));
    })
    .await;
}

#[tokio::test]
async fn tier_gate_runs_before_consent_gate() {
    // Pinned because the user spec says we never prompt the user
    // for a tool the tier would block anyway — that would be
    // noise they can't act on. A read_only tier with a fresh
    // store (no decision yet) must produce `tier_read_only`, NOT
    // `consent_required`.
    gate_test_scope(|_td| async {
        let cfg = Config::default_for_tests();
        let cfg: &'static Config = Box::leak(Box::new(cfg));
        let reg = Registry::with_only(vec![make_active_read_only_entry()]);
        let outcome =
            dispatch_tool(&reg, cfg, "fs.read_file", TIER_READ_ONLY, json!({}), false).await;
        let err = outcome.result.expect_err("tier blocks first");
        assert_eq!(err.error.code, "tier_read_only");
    })
    .await;
}

#[tokio::test]
async fn consent_gate_uses_tempfile_store_not_real_path() {
    // Smoke test: under gate_test_scope, the store writes to the
    // tempdir we injected. The host's real
    // `data/preferences/consent.json` must be untouched.
    gate_test_scope(|td| async move {
        consent::store()
            .set("any_tool", Decision::Approved)
            .expect("set");
        let written = td.path().join("consent.json");
        assert!(
            written.exists(),
            "expected injected consent.json at {}",
            written.display()
        );
    })
    .await;
}

#[test]
fn catalog_payload_lists_one_row_per_canonical_entry() {
    let reg = Registry::with_only(vec![make_active_read_only_entry(), make_deferred_entry()]);
    let cat = catalog_payload(&reg);
    assert_eq!(cat.len(), 2);
    let mut ids: Vec<String> = cat
        .iter()
        .map(|v| v["id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    assert_eq!(ids, vec!["memory_search", "read_file"]);
}

#[test]
fn catalog_payload_marks_deferred_status_with_phase() {
    let reg = Registry::with_only(vec![make_deferred_entry()]);
    let cat = catalog_payload(&reg);
    assert_eq!(cat[0]["status"], "deferred");
    assert_eq!(cat[0]["deferred_phase"], "7");
}
