//! L7 panel-walk — the changelog viewer (#196, issue #35 gate).
//!
//! Mounts the real [`ChangelogView`] in a gpui window and asserts it loads and
//! lazy-pages without panic. Unlike the nine backend-fed panels, this surface
//! makes **no** pipe calls — its content is the bundled `CHANGELOG.md` plus an
//! already-resolved headline — so the "four backend conditions" (healthy /
//! down / error / empty) all reduce to the same code path. What actually varies
//! for this component is its *content* condition: a normal changelog, an
//! empty/offline one, and one where only the headline is present. Those are the
//! conditions walked below.

use gpui::TestAppContext;
use wylde_changelog::{ChangelogView, HeadlineRelease, PAGE_SIZE};

const FIXTURE: &str = "\
# Changelog

## [0.3.0] — 2026-08-01
### Added
- newest thing

## [0.2.0] — 2026-07-01
- middle thing

## [0.1.0] — 2026-06-01
- older thing

## [0.0.9] — 2026-05-01
- oldest thing
";

#[gpui::test]
fn changelog_mounts_and_shows_first_page(cx: &mut TestAppContext) {
    let window = cx.add_window(|_w, _cx| ChangelogView::from_source(FIXTURE, None));
    cx.run_until_parked();

    window
        .update(cx, |view, _w, _cx| {
            assert_eq!(view.total_count(), 4, "one section per `## ` heading");
            assert_eq!(
                view.loaded_count(),
                PAGE_SIZE,
                "only the first page renders up front"
            );
            assert!(view.has_more(), "older versions remain to lazy-load");
        })
        .unwrap();
}

#[gpui::test]
fn changelog_lazy_loads_remaining_sections_on_demand(cx: &mut TestAppContext) {
    let window = cx.add_window(|_w, _cx| ChangelogView::from_source(FIXTURE, None));
    cx.run_until_parked();

    // Drive the same append path the scroll-wheel / "show older" affordance
    // calls, and assert it grows a page then saturates — the pagination
    // guarantee, exercised through the live entity.
    window
        .update(cx, |view, _w, cx| {
            assert_eq!(view.loaded_count(), PAGE_SIZE);
            let grew = view.load_more(cx);
            assert!(grew, "there was a fourth section to reveal");
            assert_eq!(view.loaded_count(), 4, "grew to cover all four");
            assert!(!view.has_more(), "nothing left after the last section");
            let grew_again = view.load_more(cx);
            assert!(!grew_again, "load_more is a no-op at the end");
        })
        .unwrap();
}

#[gpui::test]
fn changelog_mounts_from_the_real_bundled_file(cx: &mut TestAppContext) {
    // Exercises `new()` → the embedded CHANGELOG.md via `include_str!`, the
    // path the Shell actually mounts.
    let window = cx.add_window(|_w, cx| ChangelogView::new(None, cx));
    cx.run_until_parked();

    window
        .update(cx, |view, _w, _cx| {
            assert!(
                view.total_count() >= 1,
                "the bundled changelog yields at least one version"
            );
        })
        .unwrap();
}

#[gpui::test]
fn changelog_survives_empty_source(cx: &mut TestAppContext) {
    // Offline / unparseable changelog and no headline: a calm empty state, no
    // panic, no blank crash.
    let window = cx.add_window(|_w, _cx| ChangelogView::from_source("# Changelog\n", None));
    cx.run_until_parked();

    window
        .update(cx, |view, _w, _cx| {
            assert_eq!(view.total_count(), 0);
            assert!(!view.has_more());
        })
        .unwrap();
}

#[gpui::test]
fn changelog_shows_headline_even_when_bundle_is_empty(cx: &mut TestAppContext) {
    // The pill's release must surface even if the bundled file can't be parsed.
    let window = cx.add_window(|_w, _cx| {
        ChangelogView::from_source(
            "",
            Some(HeadlineRelease {
                version: "0.4.0".into(),
                notes: "the new release".into(),
            }),
        )
    });
    cx.run_until_parked();

    window
        .update(cx, |view, _w, _cx| {
            assert_eq!(view.total_count(), 1, "headline is shown on its own");
            assert_eq!(view.loaded_count(), 1);
        })
        .unwrap();
}
