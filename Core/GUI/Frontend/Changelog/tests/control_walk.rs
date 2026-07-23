//! L7 **control**-walk — Changelog (issue #247).
//!
//! The changelog view is a real user-facing gpui surface (#196) with one
//! interactive control: the "show older versions" pagination row. The harness
//! is `wylde_gui_test_support::control_walk`.
//!
//! This surface takes no backend, so the walk's oracle runs on the state
//! channel alone — which is the honest configuration for it, and a useful
//! proof that the harness does not silently depend on IPC traffic to detect
//! that a control did something.

use gpui::TestAppContext;

use wylde_changelog::ChangelogView;
use wylde_gui_test_support::control_walk::ControlWalk;
use wylde_gui_test_support::ScriptedBackend;

/// Four `## ` sections, so the first page leaves older ones behind and the
/// "show older" control actually paints.
const FIXTURE: &str = "\
# Changelog

## [0.2.0-beta.1] — unreleased
- newest

## [0.1.4] — 2026-06-01
- older

## [0.1.3] — 2026-05-01
- older still

## [0.1.2] — 2026-04-01
- oldest
";

/// The view's observable surface: how many sections it is currently showing.
/// Clicking "show older" grows it, which is the whole contract of that control.
fn fingerprint(v: &ChangelogView) -> String {
    format!(
        "loaded={} total={} more={}",
        v.loaded_count(),
        v.total_count(),
        v.has_more()
    )
}

#[gpui::test]
fn every_changelog_control_does_something_when_clicked(cx: &mut TestAppContext) {
    // No backend: this surface reads an embedded string. Installing the fake
    // anyway keeps the oracle's backend channel wired (it just never moves),
    // so a regression that started firing IPC from here would be visible.
    let fake = ScriptedBackend::new();
    let _guard = fake.clone().install();

    let window = cx.add_window(|_w, _cx| ChangelogView::from_source(FIXTURE, None));
    cx.run_until_parked();

    ControlWalk::new(window, &fake)
        .fingerprint(fingerprint)
        .sources(&[include_str!("../src/view.rs")])
        .run(cx)
        .assert_every_control_lives()
        .assert_covers_every_literal_id();
}

/// The pagination control is the one thing here that must not be dead — it is
/// the only way to reach anything but the newest release.
#[gpui::test]
fn the_walk_reaches_the_show_older_control(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new();
    let _guard = fake.clone().install();

    let window = cx.add_window(|_w, _cx| ChangelogView::from_source(FIXTURE, None));
    cx.run_until_parked();

    let painted = ControlWalk::new(window, &fake)
        .fingerprint(fingerprint)
        .sources(&[include_str!("../src/view.rs")])
        .run(cx)
        .painted_ids()
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>();

    assert!(
        painted.iter().any(|id| id == "wylde-changelog-load-more"),
        "the pagination control painted and was walked; got {painted:?}"
    );
}
