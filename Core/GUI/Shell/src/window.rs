//! Main-window opener.
//!
//! Slice 3 swaps the foundation-slice wordmark splash for the real
//! `Shell` view (sidebar + slot).  Window creation, the gpui platform
//! resolver, and the font hook stay where they were.
//!
//! Slice 6 adds the cross-panel nav drain: panels request
//! `wylde_gui_pipe::request_nav("core/<id>")`, the rx half lives in a
//! `Mutex<OnceCell>` here, and a gpui task pulled out of
//! `open_main_window` reads from it and calls `Shell::on_nav_click`
//! through the entity handle.
//!
//! Slice 11 (final cutover) closes the tray-drain blocker slice 10
//! surfaced.  Two shutdown entry points now both route through
//! [`crate::shutdown::run_graceful_shutdown`]:
//!
//!   * **Tray menu** — a periodic gpui task (mirroring [`spawn_nav_drain`])
//!     polls the tray's `mpsc::Receiver<TrayEvent>`.  `Quit` runs the
//!     graceful drain then `cx.quit()`; `ShowWindow` raises the window.
//!   * **Window close (X)** — `on_window_should_close` runs the same
//!     graceful drain, then quits, instead of letting gpui tear the
//!     window down and orphan the backend services.
//!
//! Both paths share the [`SHUTTING_DOWN`] latch so a second Quit / a
//! frantic double-click on the X doesn't kick off two concurrent drains.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Receiver;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use gpui::{px, App, AppContext, AsyncApp, Bounds, WindowBounds, WindowHandle, WindowOptions};

use crate::shell_root::Shell;
use crate::tray::TrayEvent;
use crate::{DEFAULT_WINDOW_HEIGHT, DEFAULT_WINDOW_WIDTH, PRODUCT_TITLE};

/// How often the tray-drain task wakes to poll the tray channel.  Tray
/// clicks are rare and the channel is a cheap `try_recv`, so 100 ms is
/// imperceptible to the user yet costs nothing measurable when idle.
const TRAY_POLL_INTERVAL: Duration = Duration::from_millis(100);

/// Process-wide latch ensuring the graceful shutdown sequence runs at
/// most once.  Both the tray `Quit` path and the window-close (X) path
/// claim it via [`claim_shutdown`]; whichever fires first wins, and the
/// loser becomes a no-op rather than racing a second drain (which would
/// double the `lifecycle.shutdown_all` dispatch and the hard-kill
/// fallback).
static SHUTTING_DOWN: AtomicBool = AtomicBool::new(false);

/// What a [`TrayEvent`] asks the shell to do.  Lifting the event→action
/// mapping out of the gpui task keeps the routing unit-testable without
/// a live window, an OS tray, or a real shutdown pipe — the same
/// two-step pattern `tray.rs::route_menu_id` already uses for the menu
/// id → `TrayEvent` half.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayAction {
    /// Run the graceful shutdown sequence, then quit the app.
    Shutdown,
    /// Raise / focus the existing main window.
    Activate,
}

/// Pure mapping from a tray event to the shell action it triggers.
pub fn tray_event_action(event: TrayEvent) -> TrayAction {
    match event {
        TrayEvent::Quit => TrayAction::Shutdown,
        TrayEvent::ShowWindow => TrayAction::Activate,
    }
}

/// Drain every pending tray event, mapping each to its [`TrayAction`].
///
/// Collection stops at the first `Shutdown` — once the user has asked
/// to quit, any events queued behind it are moot (the app is going
/// down).  Returns an empty vec when the channel is empty or has been
/// disconnected, so the caller's periodic loop just spins quietly.
pub fn drain_tray_actions(rx: &Receiver<TrayEvent>) -> Vec<TrayAction> {
    let mut actions = Vec::new();
    while let Ok(event) = rx.try_recv() {
        let action = tray_event_action(event);
        let stop = matches!(action, TrayAction::Shutdown);
        actions.push(action);
        if stop {
            break;
        }
    }
    actions
}

/// Claim the shutdown latch.  Returns `true` exactly once — for the
/// first caller — and `false` for every caller thereafter.  Pure over
/// the passed flag so the idempotency contract is testable without
/// touching the process-wide [`SHUTTING_DOWN`].
fn claim_shutdown(flag: &AtomicBool) -> bool {
    !flag.swap(true, Ordering::SeqCst)
}

/// Holds the rx half of the cross-panel nav bus until the gpui task
/// picks it up.  Locked because the take-on-startup races with the
/// (very unlikely but defensive) "tests construct two windows" path.
static NAV_RECEIVER: OnceLock<Mutex<Option<tokio::sync::mpsc::UnboundedReceiver<String>>>> =
    OnceLock::new();

/// Stash the cross-panel nav receiver so [`open_main_window`] can pick
/// it up.  Called from `main.rs` after `install_nav_sender` runs on
/// the pipe side.  Returns `false` on the second install (the cell is
/// already populated) — same idempotency contract as
/// `wylde_gui_pipe::install_nav_sender`.
pub fn install_nav_receiver(rx: tokio::sync::mpsc::UnboundedReceiver<String>) -> bool {
    NAV_RECEIVER
        .get_or_init(|| Mutex::new(None))
        .lock()
        .map(|mut slot| {
            if slot.is_some() {
                return false;
            }
            *slot = Some(rx);
            true
        })
        .unwrap_or(false)
}

/// Take the receiver out, leaving the slot empty.  Used by the gpui
/// task that drains it.
fn take_nav_receiver() -> Option<tokio::sync::mpsc::UnboundedReceiver<String>> {
    NAV_RECEIVER
        .get()
        .and_then(|m| m.lock().ok())
        .and_then(|mut slot| slot.take())
}

/// Open the main window — called once from `main.rs` after the gpui
/// app has started.  Pulls the title, dimensions, and theme from the
/// crate-level constants so a config change touches one place.
///
/// Returns the [`WindowHandle`] so the tray-drain task can raise the
/// window on a `ShowWindow` event.  `None` only when `open_window`
/// itself failed (logged), in which case the binary is unusable anyway.
pub fn open_main_window(cx: &mut App) -> Option<WindowHandle<Shell>> {
    let bounds = Bounds::centered(
        None,
        gpui::size(px(DEFAULT_WINDOW_WIDTH), px(DEFAULT_WINDOW_HEIGHT)),
        cx,
    );

    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        titlebar: Some(gpui::TitlebarOptions {
            title: Some(PRODUCT_TITLE.into()),
            ..Default::default()
        }),
        ..Default::default()
    };

    let handle = cx.open_window(options, |window, cx| {
        // Closing the window (X button) must drain the backend stack
        // first — letting gpui tear the window down would leave
        // `wylde-gateway` / `wylde-lifecycle` / … running headless.
        // We run the same graceful sequence the tray Quit uses, then
        // quit; returning `false` here cancels gpui's immediate close
        // so the app stays alive long enough to drain.
        window.on_window_should_close(cx, |_window, cx| {
            if claim_shutdown(&SHUTTING_DOWN) {
                spawn_shutdown_then_quit(cx);
            }
            // Never let gpui close the window synchronously — the
            // shutdown task owns the exit via `cx.quit()`.
            false
        });

        cx.new(|cx: &mut gpui::Context<Shell>| {
            // Pull rows out of the process-wide registry installed in
            // `main.rs`.  Falling back to an empty Shell here is fine
            // for tests that exercise `run_app` without an installed
            // registry; the live binary always installs it.
            let shell = match Shell::from_global_registry() {
                Some(s) => s,
                None => Shell::from_registry(&wylde_panel_registry::PanelRegistry::new()),
            };
            // Fire the startup probes for every `required_services`
            // member declared in the registry.  The replies arrive
            // asynchronously and call `apply_service_health` back
            // through the entity handle.
            shell.spawn_health_probes(cx);
            // Feed the sidebar's VRAM/RAM footer from the same broker
            // verb the Dashboard reads — one extra long-lived poll, no
            // new IPC surface.
            shell.spawn_resource_meter(cx);
            // Fire the one background update check (Phase 12.5, slice 3d).
            // Fire-and-forget; gated on the user's update prefs so an
            // opted-out install makes no network call.  Flips the Settings
            // row's hint dot when a newer release is found.
            shell.spawn_startup_update_check(cx);
            // Seed the update pill's per-version dismissal (#196) from the
            // persisted `skipped_version` so a previously-ignored update stays
            // dismissed across restarts (a newer release re-shows it).
            shell.spawn_seed_dismissed_version(cx);
            // Drain the cross-panel nav bus inside the Shell entity.
            // The forever-task lives as long as the entity does;
            // dropping the Shell drops the WeakEntity which short-
            // circuits the next update and the task exits.
            spawn_nav_drain(cx);
            shell
        })
    });
    cx.activate(true);

    match handle {
        Ok(h) => Some(h),
        Err(err) => {
            eprintln!("[wylde-gui] open_window failed: {err}");
            None
        }
    }
}

/// Long-lived gpui task that drains the cross-panel nav bus.  Each
/// message is a registry key like `"core/tools"`; we forward it to
/// `Shell::on_nav_click`.  Started once when the Shell entity is
/// constructed.
fn spawn_nav_drain(cx: &mut gpui::Context<Shell>) {
    let Some(mut rx) = take_nav_receiver() else {
        // No receiver installed — tests, or the binary skipped the
        // install step.  Silently no-op.
        return;
    };
    cx.spawn(async move |this, app_cx: &mut AsyncApp| {
        while let Some(key) = rx.recv().await {
            let alive = this
                .update(app_cx, |shell, cx| {
                    if !shell.on_nav_click(&key) {
                        // A panel requested navigation to a key that no nav
                        // row owns (likely a renamed/retired panel). Don't
                        // drop it silently — surface it for diagnosis.
                        eprintln!(
                            "[wylde-gui] cross-panel nav request for unknown panel key {key:?} ignored"
                        );
                    }
                    cx.notify();
                })
                .is_ok();
            if !alive {
                return;
            }
        }
    })
    .detach();
}

/// Long-lived gpui task that drains the tray menu's event channel.
///
/// Mirrors [`spawn_nav_drain`]'s shape — weak/handle termination, a
/// periodic poll — but the tray channel is a `std::sync::mpsc` (the
/// `tray-icon` callbacks fire on the OS menu thread, not tokio), so we
/// poll with `try_recv` on a gpui `background_executor().timer` tick
/// rather than `await`-ing a tokio receiver.  `ShowWindow` raises the
/// window through the captured handle; `Quit` runs the graceful
/// shutdown then exits the loop (the app is going down).
fn spawn_tray_drain(cx: &App, rx: Receiver<TrayEvent>, window: Option<WindowHandle<Shell>>) {
    cx.spawn(async move |app_cx: &mut AsyncApp| {
        loop {
            app_cx.background_executor().timer(TRAY_POLL_INTERVAL).await;

            for action in drain_tray_actions(&rx) {
                match action {
                    TrayAction::Activate => {
                        if let Some(handle) = window.as_ref() {
                            // Raise + focus.  A closed window makes this
                            // an `Err` we simply ignore — there's nothing
                            // to raise.
                            let _ = handle.update(app_cx, |_shell, window, _cx| {
                                window.activate_window();
                            });
                        }
                    }
                    TrayAction::Shutdown => {
                        // Latch so the window-close path can't also fire.
                        if claim_shutdown(&SHUTTING_DOWN) {
                            run_graceful_shutdown_then_quit(app_cx).await;
                        }
                        // Either way the app is quitting — stop draining.
                        return;
                    }
                }
            }
        }
    })
    .detach();
}

/// Spawn the graceful-drain-then-quit task from an `App` context (the
/// window-close path).  Kept separate from the tray drain because the
/// `on_window_should_close` callback hands us `&mut App`, not an
/// `AsyncApp`.
fn spawn_shutdown_then_quit(cx: &mut App) {
    cx.spawn(async move |app_cx: &mut AsyncApp| {
        run_graceful_shutdown_then_quit(app_cx).await;
    })
    .detach();
}

/// Run the graceful shutdown sequence, then quit the gpui app.  Shared
/// tail of both the tray `Quit` path and the window-close path.
async fn run_graceful_shutdown_then_quit(app_cx: &mut AsyncApp) {
    // `run_graceful_shutdown` owns the lifecycle drain + hard-kill
    // fallback; we pass a no-op `after_shutdown` and quit here so the
    // gpui `cx.quit()` runs on the foreground executor.
    crate::shutdown::run_graceful_shutdown(|| {}).await;
    // `AsyncApp::update` runs the closure on the foreground executor and
    // returns its value directly; `cx.quit()` is `()`, so there's
    // nothing to bind.  A closed app surfaces as a no-op, not an error.
    app_cx.update(|cx| cx.quit());
}

/// Start gpui's runtime.  This is the call `main.rs` ends with — once
/// it returns, the process is shutting down.
///
/// `gpui_platform::application()` resolves the OS-default `Platform`
/// implementation (DirectX 11 on Windows, Metal on macOS, Vulkan on
/// Linux); since gpui 0.2.x the `Application::new()` convenience
/// constructor is gone, so this is the canonical entry.
///
/// `tray_events` is the receiver half of the tray menu channel built in
/// `main.rs` (or `None` when the tray failed to install — the binary
/// still runs windowed, and the X button still drains via
/// `on_window_should_close`).
pub fn run_app(tray_events: Option<Receiver<TrayEvent>>) {
    gpui_platform::application()
        // Register the embedded file-tree icon bundle so `svg().path(...)`
        // resolves (visual-polish F0). Without an AssetSource gpui has nothing
        // to load path-based SVGs from.
        .with_assets(crate::assets::Assets)
        .run(move |cx: &mut App| {
            crate::assets::install_fonts();
            let window = open_main_window(cx);
            if let Some(rx) = tray_events {
                spawn_tray_drain(cx, rx, window);
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    // The render-tree tests live with the modules they exercise; this
    // file's job is just window plumbing.  Keep it free of gpui-
    // dependent assertions so a future gpui rev bump doesn't ripple
    // through the Shell crate's test surface.
    #[test]
    fn product_title_is_wylde() {
        assert_eq!(crate::PRODUCT_TITLE, "Wylde");
    }

    #[test]
    fn default_dimensions_match_spec() {
        assert_eq!(crate::DEFAULT_WINDOW_WIDTH, 1280.0);
        assert_eq!(crate::DEFAULT_WINDOW_HEIGHT, 800.0);
    }

    #[test]
    fn install_nav_receiver_round_trips_a_message() {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        // First install wins (or no-ops if a previous test claimed it).
        let _ = install_nav_receiver(rx);
        // Sender side stays usable regardless — we just don't assert
        // the message arrives through `take_nav_receiver` because the
        // OnceLock is process-wide and another test may have already
        // taken it.
        assert!(tx.send("core/dashboard".to_owned()).is_ok());
    }

    // ── Tray-drain routing (slice 11 cutover blocker) ────────────────

    #[test]
    fn quit_event_routes_to_shutdown_action() {
        // The slice-10 blocker: Quit must reach the graceful shutdown
        // path.  The routing returns `Shutdown`; the gpui task turns
        // that into `run_graceful_shutdown` + `cx.quit()`.
        assert_eq!(tray_event_action(TrayEvent::Quit), TrayAction::Shutdown);
    }

    #[test]
    fn show_window_event_routes_to_activate_action() {
        assert_eq!(
            tray_event_action(TrayEvent::ShowWindow),
            TrayAction::Activate,
        );
    }

    #[test]
    fn drain_maps_each_queued_event_in_order() {
        let (tx, rx) = std::sync::mpsc::channel::<TrayEvent>();
        tx.send(TrayEvent::ShowWindow).unwrap();
        tx.send(TrayEvent::ShowWindow).unwrap();
        let actions = drain_tray_actions(&rx);
        assert_eq!(
            actions,
            vec![TrayAction::Activate, TrayAction::Activate],
            "every queued non-quit event maps to its action, in order",
        );
    }

    #[test]
    fn drain_stops_collecting_after_a_quit() {
        // A Quit short-circuits the drain: events queued behind it are
        // moot because the app is going down.  This guarantees the gpui
        // task reaches the Shutdown branch and `return`s rather than
        // processing a stale Activate first.
        let (tx, rx) = std::sync::mpsc::channel::<TrayEvent>();
        tx.send(TrayEvent::ShowWindow).unwrap();
        tx.send(TrayEvent::Quit).unwrap();
        tx.send(TrayEvent::ShowWindow).unwrap(); // queued behind quit
        let actions = drain_tray_actions(&rx);
        assert_eq!(
            actions,
            vec![TrayAction::Activate, TrayAction::Shutdown],
            "collection halts at the first Shutdown",
        );
    }

    #[test]
    fn drain_of_empty_channel_yields_nothing() {
        let (_tx, rx) = std::sync::mpsc::channel::<TrayEvent>();
        assert!(drain_tray_actions(&rx).is_empty());
    }

    #[test]
    fn drain_of_disconnected_channel_yields_nothing() {
        let (tx, rx) = std::sync::mpsc::channel::<TrayEvent>();
        drop(tx);
        assert!(
            drain_tray_actions(&rx).is_empty(),
            "a disconnected tray channel drains to empty, not a panic",
        );
    }

    #[test]
    fn shutdown_latch_fires_exactly_once() {
        // Both the tray Quit path and the window-close path call
        // `claim_shutdown`; only the first wins so the graceful drain +
        // hard-kill fallback never run twice.  Tested over a local flag
        // so the process-wide `SHUTTING_DOWN` stays untouched.
        let flag = AtomicBool::new(false);
        assert!(claim_shutdown(&flag), "first claim wins");
        assert!(!claim_shutdown(&flag), "second claim is a no-op");
        assert!(!claim_shutdown(&flag), "and stays a no-op");
    }
}
