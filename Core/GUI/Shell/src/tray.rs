//! System-tray icon — `tray-icon` crate integration.
//!
//! Replaces `Core/GUI/src-tauri/src/tray.rs::create_tray`.  Behaviour
//! parity with the Tauri version:
//!
//!   - Tooltip is "Wylde".
//!   - Menu items: **Show Wylde**, **Quit Wylde**.
//!   - Left-click on the tray icon raises the window (toggle behaviour
//!     in the Tauri version → "raise" here, since this slice doesn't
//!     yet support hiding the window from the X button).
//!   - Quit invokes the same graceful shutdown path
//!     (`shutdown::run_graceful_shutdown`).
//!
//! `tray-icon` runs its event loop on the same thread that registers
//! the icon.  We send menu events to the gpui app via an mpsc channel
//! so the main view loop can react without blocking the OS-level
//! menu-callback thread.

use std::sync::mpsc;

/// Menu events the tray sends to the gpui event loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// "Show Wylde" — un-minimise + raise + focus the main window.
    ShowWindow,
    /// "Quit Wylde" — run `shutdown::run_graceful_shutdown` and exit.
    Quit,
}

/// Stable ids for the two menu items.  Used by the menu-event
/// dispatcher and by the unit tests below.
pub const MENU_ID_SHOW: &str = "wylde-tray-show";
pub const MENU_ID_QUIT: &str = "wylde-tray-quit";

/// Pure routing: given the menu-item id that fired, what TrayEvent
/// should the gpui app receive?  Lives outside the OS-touching code
/// so the routing is unit-testable without spinning up a real tray.
pub fn route_menu_id(id: &str) -> Option<TrayEvent> {
    match id {
        MENU_ID_SHOW => Some(TrayEvent::ShowWindow),
        MENU_ID_QUIT => Some(TrayEvent::Quit),
        _ => None,
    }
}

/// Handle owning the tray icon + the channel the gpui app drains.
/// Dropping it un-registers the OS-level icon.
///
/// We deliberately keep the OS-touching parts behind `cfg(target_os)`
/// guards inside this module rather than scattering them: `try_install`
/// is the single entry point, and on a platform we don't support it
/// returns `Err`.
pub struct TrayHandle {
    pub events: mpsc::Receiver<TrayEvent>,
    // The underlying `tray-icon` resource keeps the icon alive on the OS.
    // Held opaquely so the type stays platform-neutral at the boundary
    // even if `tray-icon`'s public type names differ per platform in a
    // future release.
    _inner: Inner,
}

#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
struct Inner {
    _icon: tray_icon::TrayIcon,
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
struct Inner;

/// Attempt to install the tray icon and its context menu.  Returns
/// the handle (which the caller stashes on the App so the icon's
/// lifetime tracks the app's) or an error string suitable for logging.
///
/// `icon_path` should point at a `.png` / `.ico` that the OS will use
/// for the tray glyph.  The Tauri-era icons under
/// `Core/GUI/src-tauri/icons/` are the obvious reuse target.
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
pub fn try_install(icon_path: &std::path::Path) -> Result<TrayHandle, String> {
    use tray_icon::menu::{Menu, MenuEvent, MenuItem};
    use tray_icon::{TrayIconBuilder, TrayIconEvent};

    let icon = load_icon(icon_path)?;

    let menu = Menu::new();
    let show = MenuItem::with_id(MENU_ID_SHOW, "Show Wylde", true, None);
    let quit = MenuItem::with_id(MENU_ID_QUIT, "Quit Wylde", true, None);
    menu.append(&show).map_err(|e| e.to_string())?;
    menu.append(&quit).map_err(|e| e.to_string())?;

    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_tooltip(crate::PRODUCT_TITLE)
        .with_icon(icon)
        .build()
        .map_err(|e| e.to_string())?;

    let (tx, rx) = mpsc::channel();

    // Menu events fire on the tray-icon thread; forward them via the
    // routing function so the gpui side gets typed `TrayEvent`s.
    let menu_tx = tx.clone();
    MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
        if let Some(routed) = route_menu_id(event.id.as_ref()) {
            let _ = menu_tx.send(routed);
        }
    }));

    // Left-click on the tray icon raises the main window — same
    // behaviour as the Tauri tray's "left click → toggle" except we
    // currently only support raise (window hide-on-X lands later).
    TrayIconEvent::set_event_handler(Some(move |event: TrayIconEvent| {
        if let TrayIconEvent::Click {
            button: tray_icon::MouseButton::Left,
            button_state: tray_icon::MouseButtonState::Up,
            ..
        } = event
        {
            let _ = tx.send(TrayEvent::ShowWindow);
        }
    }));

    Ok(TrayHandle {
        events: rx,
        _inner: Inner { _icon: tray },
    })
}

#[cfg(not(any(target_os = "windows", target_os = "macos", target_os = "linux")))]
pub fn try_install(_icon_path: &std::path::Path) -> Result<TrayHandle, String> {
    Err("tray-icon: unsupported platform".to_string())
}

/// Decode a PNG / ICO from disk into the tray crate's icon format.
#[cfg(any(target_os = "windows", target_os = "macos", target_os = "linux"))]
fn load_icon(path: &std::path::Path) -> Result<tray_icon::Icon, String> {
    use std::io::Read;

    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|e| format!("open tray icon {}: {}", path.display(), e))?
        .read_to_end(&mut bytes)
        .map_err(|e| format!("read tray icon {}: {}", path.display(), e))?;

    // `tray_icon::Icon::from_path` would also work; reading bytes
    // ourselves keeps the error reporting consistent across PNG/ICO
    // formats and makes the path traceable in logs.
    tray_icon::Icon::from_path(path, None)
        .map_err(|e| format!("decode tray icon {}: {}", path.display(), e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn routes_show_menu_id() {
        assert_eq!(route_menu_id(MENU_ID_SHOW), Some(TrayEvent::ShowWindow));
    }

    #[test]
    fn routes_quit_menu_id_to_shutdown_path() {
        // The slice spec requires the Quit menu to invoke the graceful
        // shutdown path.  The routing returns `TrayEvent::Quit`; the
        // shell binary translates that into `run_graceful_shutdown`.
        // Two-step routing means the routing layer stays free of any
        // OS handles and the unit test doesn't need a real pipe.
        assert_eq!(route_menu_id(MENU_ID_QUIT), Some(TrayEvent::Quit));
    }

    #[test]
    fn ignores_unknown_menu_id() {
        assert_eq!(route_menu_id("some-other-id"), None);
        assert_eq!(route_menu_id(""), None);
    }

    /// Sanity that the menu ids are distinct — if a future rename
    /// accidentally collides them, every click acts as Quit.
    #[test]
    fn menu_ids_are_distinct() {
        assert_ne!(MENU_ID_SHOW, MENU_ID_QUIT);
    }
}
