//! Wylde GUI — gpui-era shell.  Library surface so the orchestration
//! pieces (shutdown sequence, tray-menu routing, asset loading, the
//! nav/slot model) can be unit-tested without the gpui window event
//! loop in scope.
//!
//! The runnable binary lives in `main.rs`; everything reusable +
//! testable lives in the modules below.

pub mod assets;
pub mod nav;
pub mod pack;
pub mod shell_root;
pub mod shutdown;
pub mod sidebar;
pub mod slot;
pub mod tray;
pub mod window;

/// Canonical product title — used in the window title, tray tooltip,
/// and (later) the autostart entry.  Centralised so a rebrand changes
/// one string.
pub const PRODUCT_TITLE: &str = "Wylde";

/// Default window dimensions.  Matches the existing Tauri config
/// (`tauri.conf.json` width=1280, height=860); the spec rounded that
/// to 1280×800 and either is fine — we honour the spec.
pub const DEFAULT_WINDOW_WIDTH: f32 = 1280.0;
pub const DEFAULT_WINDOW_HEIGHT: f32 = 800.0;
