//! L7 panel-walk — Chat (issue #35, roadmap T0.1b).
//!
//! Chat is the most heavily covered panel already (dock scoping, conversations,
//! virtualization, processing indicator). This file is the uniform
//! mount-under-every-backend-condition smoke the panel-walk gate runs across
//! all 9 panels — its job here is to pin that the Chat surface MOUNTS and loads
//! its conversation list under every realistic backend condition without
//! panicking (a panel that panics when the daemon is down is exactly the
//! shipped-broken class this gate exists to catch).
//!
//! **What "error state" means for Chat:** `error: Option<String>` is reserved
//! for *action* failures (start/delete/eject/consent); the mount-time loaders
//! degrade silently, so the contract at mount is `error.is_none()` and a
//! coherent (possibly empty) conversation list — no panic, no stuck state. The
//! "services not running" copy is a render-time affordance, not this field.
//!
//! Backend conditions: healthy · down/unavailable · error envelope · empty.

use gpui::TestAppContext;
use serde_json::{json, Value};

use wylde_gui_test_support::ScriptedBackend;
use wylde_panel_chat::chat_panel::{ChatPanel, ChatScope};

fn conv(id: &str, ws: &str, updated: i64) -> Value {
    json!({ "id": id, "workspace_id": ws, "updated_at": updated, "title": "t" })
}

/// Mount a Docked ChatPanel the way `ChatPanel::docked` does (bare `new` + the
/// public mount loaders), then scope it into a workspace to drive the scoped
/// conversation-list load — all through the fake backend.
fn mount(cx: &mut TestAppContext) -> gpui::WindowHandle<ChatPanel> {
    let window = cx.add_window(|_w, cx| {
        let panel = ChatPanel::new(ChatScope::Docked, cx);
        ChatPanel::spawn_load_workspaces(cx);
        ChatPanel::spawn_load_models(cx);
        ChatPanel::spawn_load_reasoning(cx);
        panel
    });
    cx.run_until_parked();
    window
        .update(cx, |panel, _w, cx| {
            panel.apply_workspace_scope(Some("ws-a".to_owned()), cx);
        })
        .unwrap();
    cx.run_until_parked();
    window
}

#[gpui::test]
fn chat_healthy_mounts_and_loads(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().conversations(vec![conv("c1", "ws-a", 200)]);
    let _guard = fake.clone().install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(panel.error.is_none(), "no error on the happy path");
            assert_eq!(
                panel.active_workspace_id.as_deref(),
                Some("ws-a"),
                "the dock scoped into the workspace"
            );
        })
        .unwrap();
}

#[gpui::test]
fn chat_survives_backend_down(cx: &mut TestAppContext) {
    // Every mount-time read fails. The Chat dock must still mount and stay
    // coherent (the "services not running" state is a render affordance) — it
    // must NOT panic.
    let fake = ScriptedBackend::new()
        .on_err(
            "conversations.list",
            "pipe_unavailable: harness not running",
        )
        .on_err(
            "workspaces.list_mru",
            "pipe_unavailable: workspaces not running",
        )
        .on_err(
            "models.get_effective",
            "pipe_unavailable: harness not running",
        );
    let _guard = fake.install();

    let window = mount(cx);

    // Reaching this assertion at all proves the mount + scoped load didn't
    // panic when every backend was down.
    window
        .update(cx, |panel, _w, _cx| {
            assert!(
                panel.error.is_none(),
                "the dock degrades silently, no mount-time banner"
            );
        })
        .unwrap();
}

#[gpui::test]
fn chat_surfaces_backend_error_envelope(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new().on_err(
        "conversations.list",
        "internal_error: conversation store blew up",
    );
    let _guard = fake.install();

    let window = mount(cx);

    // Reaching this closure proves the mount + scoped load survived the error
    // envelope without panicking; the list simply stays empty.
    window
        .update(cx, |panel, _w, _cx| {
            assert!(
                panel.error.is_none(),
                "mount loaders don't banner on an error envelope"
            );
            assert!(
                panel.conversations.is_empty(),
                "a failed list load leaves an empty list"
            );
        })
        .unwrap();
}

#[gpui::test]
fn chat_tolerates_empty_backend(cx: &mut TestAppContext) {
    let fake = ScriptedBackend::new();
    let _guard = fake.install();

    let window = mount(cx);

    window
        .update(cx, |panel, _w, _cx| {
            assert!(
                panel.error.is_none(),
                "an empty backend is not an error for Chat"
            );
            assert!(
                panel.conversations.is_empty(),
                "no threads yet is a clean empty state"
            );
        })
        .unwrap();
}
