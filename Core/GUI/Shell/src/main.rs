//! Wylde GUI binary — gpui-era replacement for `fletch-gui.exe`.
//!
//! Slice 3 wires up:
//!   * `PanelRegistry::install_global()` so the sidebar / slot have
//!     panels to render.
//!   * Health probes against every `required_services` member declared
//!     by the registered panels (one `service.health` call per
//!     service, fired once at startup; the `Start service` button on
//!     the stub re-probes per click).
//!   * The Settings panel's `refresh` task — Slice 2's TODO #3.  Once
//!     the gpui app is up we spawn a task that fires the panel's IPC
//!     reads and writes the resulting state back through the View's
//!     entity handle.
//!
//! Slice 11 (final cutover) wires the last missing piece: the tray-event
//! drain inside the gpui loop.  `window::run_app` now takes the tray
//! receiver and spawns a periodic drain task that routes `Quit` →
//! `shutdown::run_graceful_shutdown` → `cx.quit()` and `ShowWindow` →
//! raise-window; the window-close (X) path runs the same graceful drain
//! via `on_window_should_close`.
//!
//! Pieces still post-alpha:
//!   * Updater + auto-launch + installer (the slice-spec pin-point).

use std::sync::Arc;

use wylde_gui::assets;
use wylde_gui::tray::{self, TrayEvent};
use wylde_panel_registry::{factories::default_first_party, generated, PanelRegistry};

fn main() {
    // 0) Build the process-wide panel registry BEFORE the gpui app
    //    runs.  Slice 2 left this as a TODO; without it the Shell
    //    falls back to an empty registry.
    install_panel_registry();

    // 1) Install the tray icon BEFORE the gpui event loop starts.
    //    tray-icon's OS handles bind to whichever thread created them;
    //    keeping that the main thread matches the gpui idiom and
    //    avoids a cross-thread tray construction race.
    let tray_handle = match assets::locate_tray_icon() {
        Some(icon_path) => match tray::try_install(&icon_path) {
            Ok(handle) => Some(handle),
            Err(err) => {
                eprintln!("[wylde-gui] tray install failed ({err}); continuing without tray");
                None
            }
        },
        None => {
            eprintln!(
                "[wylde-gui] could not locate tray icon (looked for `assets/icons/icon.{{ico,png}}` \
                 walking up from {:?}); continuing without tray",
                std::env::current_exe().ok()
            );
            None
        }
    };

    // 2) Pull the tray's mpsc receiver out so the gpui side can drain
    //    it.  The receiver is `Send` (so it can move into gpui's
    //    foreground task) but `!Sync`; the gpui foreground executor
    //    boxes the drain task `local`, so a bare `Receiver` is fine —
    //    no `Arc<Mutex<…>>` wrapper needed.  `None` when the tray
    //    failed to install; the window-close path still drains.
    let tray_events: Option<std::sync::mpsc::Receiver<TrayEvent>> =
        tray_handle.map(|h| h.events);

    // 3) Spin up a tokio runtime for all pipe IO.  gpui owns the main
    //    event loop; its dispatcher threads have no current tokio
    //    runtime, so the named-pipe client + `tokio::time::timeout`
    //    used by `wylde_gui_pipe::call` would panic if called
    //    directly.  We stash a Handle into the pipe crate so every
    //    `cx.spawn`'d task transparently hops to this runtime for
    //    the IO portion (see `wylde_gui_pipe::install_runtime`).
    let tokio_runtime = Arc::new(
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .expect("build tokio runtime"),
    );
    wylde_gui_pipe::install_runtime(tokio_runtime.handle().clone());
    // (Held over the gpui run to keep its threads alive while the gpui
    // loop drives the UI.)
    let _runtime_keepalive = tokio_runtime.clone();

    // 3.5) Install the cross-panel nav bus.  Panels call
    //      `wylde_gui_pipe::request_nav("core/<id>")` to ask the Shell
    //      to switch tabs; the Shell drains the rx inside its gpui
    //      task and calls `NavModel::select`.  The bus lives in the
    //      pipe crate to avoid a registry-deps-on-panel-deps-on-registry
    //      cycle.
    let (nav_tx, nav_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    wylde_gui_pipe::install_nav_sender(nav_tx);
    wylde_gui::window::install_nav_receiver(nav_rx);

    // 4) Start the gpui event loop.  Returns when the user quits.
    //    The tray receiver moves in so the periodic tray-drain task can
    //    route Quit → graceful shutdown and ShowWindow → raise-window.
    wylde_gui::window::run_app(tray_events);
}

/// Build the static `PanelRegistry` from the generated `register_all`
/// hook + the hand-maintained `factories.rs` table, then install it
/// process-wide.  Panics on a duplicate or missing factory — the
/// failure mode the slice spec calls for ("loud at startup, not a
/// silent missing tab at runtime").
fn install_panel_registry() {
    let mut registry = PanelRegistry::new();
    let mut factories = default_first_party();
    if let Err(err) = generated::register_all(&mut registry, &mut factories) {
        // A panicking abort is fine — the binary is non-functional
        // without a registry, and a printed error walks the user
        // straight to the broken manifest/factory pair.
        panic!("panel-registry bootstrap failed: {err}");
    }
    if !registry.install_global() {
        // `install_global` returns false on the second call; in the
        // live binary this should never happen because `main` runs
        // once.  Swallow rather than panic so a hot-reload tool that
        // re-enters `main` isn't catastrophic.
        eprintln!(
            "[wylde-gui] panel registry already installed; ignoring duplicate install request",
        );
    }
}
