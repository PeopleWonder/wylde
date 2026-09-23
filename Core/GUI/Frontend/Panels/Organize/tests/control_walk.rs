//! L7 **control**-walk: Organize (the #247 standard, for #39).
//!
//! Harness: `wylde_gui_test_support::control_walk`. Every clickable control is
//! built with `control(div(), id)`, so the walk enumerates and clicks each one,
//! and a click must move at least one observable channel (a backend call or the
//! panel fingerprint).
//!
//! Three frames cover every control:
//! * **default**: the scope picker, Scan and Undo;
//! * **broad scope**: the opt-in toggle only paints for the profile/drive tiers;
//! * **plan review**: the per-op / per-removal Keep-Skip toggles and Apply only
//!   paint once a scan has returned a plan.
//!
//! `tier-user_data` is the scope radio's active segment in the reset frame, so
//! clicking it is a deliberate no-op. It is declared `external_effect`, the
//! same treatment every other radio's active segment gets (GraphSettings
//! Layout, the Devices tier row); the other two segments prove the wiring.

use std::sync::Arc;

use gpui::TestAppContext;
use serde_json::json;

use wylde_gui_test_support::control_walk::{ControlWalk, WalkReport};
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

fn backend() -> Arc<ScriptedBackend> {
    ScriptedBackend::new()
        .on("organize.propose", sample_plan())
        .on(
            "organize.apply",
            json!({ "applied": 2, "skipped": 0, "failed": 0, "undo_token": "plan-1" }),
        )
        .on(
            "organize.undo",
            json!({ "plan_id": "plan-1", "restored": 2, "skipped": 0, "failed": 0 }),
        )
}

/// Everything a control can change: the scope picker, the opt-in, the curated
/// plan, and the status/undo lines the async calls land in.
fn fingerprint(p: &OrganizePanel) -> String {
    format!(
        "tier={:?} opt_in={} plan={} rej_ops={} rej_rem={} loading={} err={:?} status={:?} undo={:?}",
        p.tier,
        p.opt_in,
        p.proposal.is_some(),
        p.rejected_ops.len(),
        p.rejected_removals.len(),
        p.loading,
        p.error,
        p.status,
        p.last_undo_token,
    )
}

/// Back to a fresh panel before every click, so each control is judged from
/// the same baseline (the user-data tier, nothing scanned).
fn reset(p: &mut OrganizePanel, _w: &mut gpui::Window, cx: &mut gpui::Context<OrganizePanel>) {
    p.tier = TierUi::UserData;
    p.opt_in = false;
    p.proposal = None;
    p.rejected_ops.clear();
    p.rejected_removals.clear();
    p.loading = false;
    p.error = None;
    p.status = None;
    p.last_undo_token = None;
    cx.notify();
}

fn mount(cx: &mut TestAppContext) -> gpui::WindowHandle<OrganizePanel> {
    let window = cx.add_window(|_w, cx| OrganizePanel::new(cx));
    cx.run_until_parked();
    window
}

fn walk(
    cx: &mut TestAppContext,
    window: gpui::WindowHandle<OrganizePanel>,
    fake: &Arc<ScriptedBackend>,
) -> WalkReport {
    ControlWalk::new(window, fake)
        .fingerprint(fingerprint)
        .reset(reset)
        .external_effect(&["tier-user_data"])
        .state("broad scope", |p: &mut OrganizePanel, _w, cx| {
            p.set_tier(TierUi::UserProfile, cx)
        })
        .state("plan review", |p: &mut OrganizePanel, _w, cx| p.scan(cx))
        .sources(&[include_str!("../src/organize_panel.rs")])
        .run(cx)
}

#[gpui::test]
fn every_organize_control_does_something_when_clicked(cx: &mut TestAppContext) {
    let fake = backend();
    let _guard = fake.clone().install();
    let window = mount(cx);

    walk(cx, window, &fake)
        .assert_every_control_lives()
        .assert_covers_every_literal_id();
}

#[gpui::test]
fn the_walk_reaches_the_opt_in_and_plan_review_controls(cx: &mut TestAppContext) {
    let fake = backend();
    let _guard = fake.clone().install();
    let window = mount(cx);

    let report = walk(cx, window, &fake);
    let painted = report.painted_ids();
    for id in [
        "organize-scan",
        "organize-undo",
        "tier-user_profile",
        "tier-drive",
        "organize-optin",
        "op-1",
        "op-2",
        "organize-apply",
    ] {
        assert!(
            painted.contains(&id),
            "{id} painted and was walked; got {painted:?}"
        );
    }
}
