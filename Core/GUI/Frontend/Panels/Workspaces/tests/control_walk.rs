//! L7 **control**-walk — Workspaces (issue #247).
//!
//! Workspaces is a container of independently-mountable sub-views. This file
//! walks the **Hierarchy** sub-view, which is self-contained: all of its
//! controls act on its own state, so it walks cleanly when mounted alone.
//!
//! The other surfaces are staged deliberately:
//!
//! * **Concepts / Relations / Vocabulary** render a sub-tab switcher whose
//!   click switches the *parent* `VocabularyTab`'s active tab. Mounted
//!   standalone they have no parent to switch, so those pills produce no
//!   observable effect in isolation — they are not dead, they need the
//!   container. They are walked through `VocabularyTab` in a follow-up
//!   (#247, Workspaces walk part 2), where the tab state is real.
//! * The **graph canvas** + the main `WorkspacesPanel` chrome carry heavier
//!   IPC + physics state and are walked there too.
//!
//! All 13 Workspaces source files are already routed through `control()` (this
//! batch), so `wylde_check` rule 59 enforces every one of them; the staged
//! work above is about *walking* them, not routing them.

use gpui::TestAppContext;
use serde_json::{json, Value};

use wylde_gui_test_support::control_walk::ControlWalk;
use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_workspaces::hierarchy::HierarchyView;

fn tree_reply() -> Value {
    json!({
        "workspace_id": "ws-a",
        "enabled": true,
        "roots": [{
            "concept_id": "root", "label": "Root", "children": [
                { "concept_id": "child", "label": "Child", "children": [] }
            ]
        }],
        "unplaced": [{ "concept_id": "loose", "label": "Loose" }]
    })
}

#[gpui::test]
fn every_hierarchy_control_does_something(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new()
        .on(
            "workspaces.list_mru",
            json!({ "active_id": "ws-a", "workspaces": [{ "id": "ws-a", "folder": "C:/code/a" }] }),
        )
        .on("workspaces.hierarchy.get_tree", tree_reply());
    let _guard = fake.clone().install();
    let window = cx.add_window(|_w, cx| HierarchyView::new(cx));
    cx.run_until_parked();

    ControlWalk::new(window, &fake)
        .fingerprint(|v: &HierarchyView| {
            format!(
                "loading={} enabled={} ws={:?}",
                v.is_loading(),
                v.is_enabled(),
                v.workspace_id(),
            )
        })
        .sources(&[include_str!("../src/hierarchy/mod.rs")])
        .run(cx)
        .assert_every_control_lives()
        .assert_covers_every_literal_id();
}
