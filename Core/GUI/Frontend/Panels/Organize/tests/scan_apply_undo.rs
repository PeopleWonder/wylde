//! Windowed gpui tests for the Organize panel — mount a real `OrganizePanel`
//! and drive scan → review → apply → undo through the scripted fake backend at
//! the `wylde_gui_pipe::call` seam (no live `wylde-organize` service).
//!
//! The key assertion: **Apply sends only the accepted (non-rejected) ops and
//! removals** — the per-row Keep/Skip toggles actually curate the plan that
//! reaches the service.

use gpui::TestAppContext;
use serde_json::json;

use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_organize::organize_panel::TierUi;
use wylde_panel_organize::OrganizePanel;

fn sample_plan() -> serde_json::Value {
    json!({
        "plan_id": "plan-1",
        "scope_tier": "user_data",
        "roots": ["C:/Users/x/Downloads"],
        "ops": [
            { "id": 1, "kind": "mkdir", "to": "C:/Users/x/Downloads/Documents", "rationale": "category folder", "confidence": 0.97 },
            { "id": 2, "kind": "move", "from": "C:/Users/x/Downloads/a.pdf", "to": "C:/Users/x/Downloads/Documents/a.pdf", "rationale": "group", "confidence": 0.95 }
        ],
        "removals": [
            { "path": "C:/Users/x/Downloads/old.tmp", "reason": "temp", "size": 12, "detail": "temp/scratch file" }
        ],
        "skipped": [{ "path": "C:/Windows", "reason": "protected: os_directory" }],
        "stats": { "files_scanned": 2, "ops_proposed": 2, "removals_proposed": 1, "skipped_protected": 1, "reclaimable_bytes": 12 }
    })
}

#[gpui::test]
fn scan_renders_the_proposed_plan(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on("organize.propose", sample_plan());
    let _g = fake.clone().install();
    let window = cx.add_window(|_w, cx| OrganizePanel::new(cx));
    cx.run_until_parked();

    window.update(cx, |p, _w, cx| p.scan(cx)).unwrap();
    cx.run_until_parked();

    assert_eq!(fake.count_for("organize.propose"), 1);
    window
        .update(cx, |p, _w, _cx| {
            let prop = p.proposal.as_ref().expect("a plan was rendered");
            assert_eq!(prop.view.ops.len(), 2);
            assert_eq!(prop.view.removals.len(), 1);
            assert_eq!(prop.view.skipped.len(), 1);
            assert!(p.status.is_some());
            assert!(p.error.is_none());
        })
        .unwrap();
}

#[gpui::test]
fn apply_sends_only_the_accepted_ops_and_removals(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on("organize.propose", sample_plan())
        .on(
            "organize.apply",
            json!({ "applied": 1, "skipped": 0, "failed": 0, "undo_token": "plan-1" }),
        );
    let _g = fake.clone().install();
    let window = cx.add_window(|_w, cx| OrganizePanel::new(cx));
    cx.run_until_parked();

    window.update(cx, |p, _w, cx| p.scan(cx)).unwrap();
    cx.run_until_parked();

    // Reject op #2 (the move) and the lone removal.
    window
        .update(cx, |p, _w, cx| {
            p.toggle_op(2, cx);
            p.toggle_removal("C:/Users/x/Downloads/old.tmp".to_string(), cx);
        })
        .unwrap();
    window.update(cx, |p, _w, cx| p.apply(cx)).unwrap();
    cx.run_until_parked();

    let call = fake
        .last_call_for("organize.apply")
        .expect("Apply dispatched organize.apply");
    let ops = call.payload["plan"]["ops"].as_array().expect("ops array");
    assert_eq!(ops.len(), 1, "the rejected op was dropped from the curated plan");
    assert_eq!(ops[0]["id"], 1, "only the accepted mkdir survived");
    let rems = call.payload["plan"]["removals"].as_array().expect("removals array");
    assert!(rems.is_empty(), "the rejected removal was dropped");

    window
        .update(cx, |p, _w, _cx| {
            assert!(p.proposal.is_none(), "the review surface clears after apply");
            assert_eq!(p.last_undo_token.as_deref(), Some("plan-1"));
            assert!(p.status.is_some());
        })
        .unwrap();
}

#[gpui::test]
fn undo_dispatches_the_token(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on(
        "organize.undo",
        json!({ "plan_id": "plan-1", "restored": 2, "skipped": 0, "failed": 0 }),
    );
    let _g = fake.clone().install();
    let window = cx.add_window(|_w, cx| OrganizePanel::new(cx));
    cx.run_until_parked();

    window.update(cx, |p, _w, cx| p.undo(cx)).unwrap();
    cx.run_until_parked();

    assert_eq!(fake.count_for("organize.undo"), 1);
    let call = fake.last_call_for("organize.undo").unwrap();
    assert_eq!(
        call.payload_str("undo_token").as_deref(),
        Some("latest"),
        "with no prior apply, Undo targets the latest journaled plan"
    );
    window
        .update(cx, |p, _w, _cx| assert!(p.status.is_some()))
        .unwrap();
}

#[gpui::test]
fn a_scope_gate_error_is_surfaced_not_swallowed(cx: &mut TestAppContext) {
    // The service refuses a drive scan without the typed phrase; the panel must
    // surface that as a visible error, not a silent no-op.
    let fake = ScriptedBackend::new().on_err(
        "organize.propose",
        "scope_typed_confirmation_required: type the phrase to confirm",
    );
    let _g = fake.clone().install();
    let window = cx.add_window(|_w, cx| OrganizePanel::new(cx));
    cx.run_until_parked();

    window
        .update(cx, |p, _w, cx| {
            p.set_tier(TierUi::Drive, cx);
            p.scan(cx);
        })
        .unwrap();
    cx.run_until_parked();

    window
        .update(cx, |p, _w, _cx| {
            assert!(p.error.is_some(), "the scope-gate refusal is shown");
            assert!(p.proposal.is_none(), "no plan rendered on a refused scan");
        })
        .unwrap();
}
