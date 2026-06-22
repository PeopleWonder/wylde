//! Windowed gpui tests for the curate-before-inject menu (concept-routing
//! **R2**, plan §4). Mount the real [`CurateMenuView`] in a test window, drive
//! it through the scripted fake backend at the `wylde_gui_pipe::call`
//! chokepoint (the `chat.preview_context` harness verb), and assert the
//! two-phase flow + cadence — no live harness. See `docs/gui-testing.md`.

use gpui::TestAppContext;
use serde_json::{json, Value};

use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_workspaces::routing::{CurateMenuView, TurnDecision};

/// A `chat.preview_context` reply: routing on, interactive, with one activated
/// concept, one auto-pulled dependency, and one excluded concept.
fn preview_reply() -> Value {
    json!({
        "routing_enabled": true,
        "curate": true,
        "inject_token_budget": 1500,
        "candidates": {
            "query_echo": "how does nextcloud sync",
            "concepts": [
                { "id": "nextcloud", "label": "Nextcloud", "score": 0.71, "seed_score": 0.71,
                  "provenance": { "kind": "seed" }, "activated": true },
                { "id": "ddns", "label": "DDNS", "score": 0.32, "seed_score": 0.30,
                  "provenance": { "kind": "dependency", "from": {"node":"concept","id":"nextcloud"}, "hops": 1 },
                  "activated": true },
                { "id": "wylde", "label": "Wylde", "score": 0.30, "seed_score": 0.62,
                  "provenance": { "kind": "inhibited", "by": {"node":"concept","id":"nextcloud"}, "raw": 0.62 },
                  "activated": false }
            ],
            "vocabulary": [ { "identifier": "the_pipe", "score": 1.0 } ],
            "abs_threshold": 0.5, "chosen_cutoff": 0.5, "activated_count": 2, "max_concepts": 3
        }
    })
}

fn load(window: &gpui::WindowHandle<CurateMenuView>, cx: &mut TestAppContext, conv: &str) {
    let conv = conv.to_owned();
    window
        .update(cx, |v, _w, cx| {
            v.load_for_turn(
                "ws-a".into(),
                conv,
                "how does nextcloud sync".into(),
                None,
                cx,
            );
        })
        .unwrap();
    cx.run_until_parked();
}

#[gpui::test]
fn first_turn_opens_menu_with_activated_pre_checked(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on("chat.preview_context", preview_reply());
    let _guard = fake.clone().install();

    let window = cx.add_window(|_w, cx| CurateMenuView::new(cx));
    load(&window, cx, "conv1");

    window
        .update(cx, |v, _w, _cx| {
            assert!(v.is_open(), "first turn opens the menu (never silent)");
            assert_eq!(*v.decision(), TurnDecision::Prompt);
            let model = v.model().expect("menu model");
            // Pre-checked = the activated set (concept + auto-pulled dependency).
            assert_eq!(
                model.checked_concepts(),
                vec!["nextcloud".to_owned(), "ddns".to_owned()]
            );
            // Vocab is shown but not in the curated set.
            assert!(model.rows.iter().any(|r| r.label == "{{the_pipe}}"));
        })
        .unwrap();
    assert!(fake.count_for("chat.preview_context") >= 1);
}

#[gpui::test]
fn curate_then_inject_resolves_the_selection(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on("chat.preview_context", preview_reply());
    let _guard = fake.clone().install();
    let window = cx.add_window(|_w, cx| CurateMenuView::new(cx));
    load(&window, cx, "conv1");

    // Remove the auto-pulled dependency, then confirm.
    window
        .update(cx, |v, _w, cx| {
            v.toggle("ddns", cx);
            v.confirm_inject(cx);
        })
        .unwrap();

    window
        .update(cx, |v, _w, _cx| {
            assert!(!v.is_open(), "menu closes on confirm");
            assert_eq!(
                v.resolved_selection(),
                Some(&vec!["nextcloud".to_owned()]),
                "curated list = the checked set after the user's edit"
            );
        })
        .unwrap();
}

#[gpui::test]
fn skip_injects_nothing(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on("chat.preview_context", preview_reply());
    let _guard = fake.clone().install();
    let window = cx.add_window(|_w, cx| CurateMenuView::new(cx));
    load(&window, cx, "conv1");

    window.update(cx, |v, _w, cx| v.skip(cx)).unwrap();
    window
        .update(cx, |v, _w, _cx| {
            assert_eq!(
                v.resolved_selection(),
                Some(&Vec::<String>::new()),
                "Skip ⇒ inject nothing"
            );
        })
        .unwrap();
}

#[gpui::test]
fn auto_next_then_later_turn_auto_applies_without_menu(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on("chat.preview_context", preview_reply());
    let _guard = fake.clone().install();
    let window = cx.add_window(|_w, cx| CurateMenuView::new(cx));

    // Turn 1: menu opens; user picks the default set + "⟳ auto next time".
    load(&window, cx, "conv1");
    window.update(cx, |v, _w, cx| v.auto_next(cx)).unwrap();
    window
        .update(cx, |v, _w, _cx| {
            assert!(!v.is_open());
            assert_eq!(
                v.resolved_selection(),
                Some(&vec!["nextcloud".to_owned(), "ddns".to_owned()])
            );
        })
        .unwrap();

    // Turn 2 (same conversation): auto-applies the remembered set, no menu.
    load(&window, cx, "conv1");
    window
        .update(cx, |v, _w, _cx| {
            assert!(!v.is_open(), "later turn auto-applies — no blocking menu");
            assert_eq!(
                *v.decision(),
                TurnDecision::AutoInject(vec!["nextcloud".to_owned(), "ddns".to_owned()])
            );
        })
        .unwrap();

    // Re-open control: forces the menu on the next turn.
    window.update(cx, |v, _w, cx| v.reopen(cx)).unwrap();
    load(&window, cx, "conv1");
    window
        .update(cx, |v, _w, _cx| {
            assert!(v.is_open(), "re-open forces the menu back");
        })
        .unwrap();
}

#[gpui::test]
fn toggle_off_routing_runs_plain_turn(cx: &mut TestAppContext) {
    // Master toggle OFF ⇒ no menu, plain turn (today's behaviour).
    let fake = ScriptedBackend::new().on(
        "chat.preview_context",
        json!({ "routing_enabled": false, "curate": false, "candidates": null, "inject_token_budget": 1500 }),
    );
    let _guard = fake.clone().install();
    let window = cx.add_window(|_w, cx| CurateMenuView::new(cx));
    load(&window, cx, "conv1");

    window
        .update(cx, |v, _w, _cx| {
            assert!(!v.is_open(), "toggle off ⇒ no menu");
            assert_eq!(*v.decision(), TurnDecision::PlainTurn);
            // Plain turn carries no curated concepts.
            assert_eq!(v.resolved_selection(), Some(&Vec::<String>::new()));
        })
        .unwrap();
}
