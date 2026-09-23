//! The Shell's **nav chrome** — the left sidebar, the panel slot (with its
//! service-recovery affordance), and the bottom-left update pill + changelog
//! modal — plus their pure view-model ([`nav`]) and render helpers ([`pack`],
//! [`resource_meter`]).
//!
//! Extracted from the `wylde-gui` (Shell) crate, which links `wry` (webview)
//! and `tray-icon` and so cannot be built by the headless L7 `panel-walk` job.
//! None of these renderers actually touch `wry`/tray — their only coupling was
//! the concrete `Shell` type, replaced here by the small [`NavChromeHost`]
//! trait the Shell implements. That makes the whole nav chrome a `wry`-free
//! crate the panel-walk can build and **control-walk** (#247).
//!
//! The Shell re-consumes everything here and adds one
//! `impl NavChromeHost for Shell` block (pure delegation to its existing
//! methods); nothing else about the Shell changes.

/// The product name shown in the sidebar brand header. Mirrors the Shell
/// crate's own `PRODUCT_TITLE` (the sidebar renders it; the Shell uses its copy
/// for the window/tray title).
pub const PRODUCT_TITLE: &str = "Wylde";

pub mod host;
pub mod nav;
pub mod pack;
pub mod resource_meter;
pub mod sidebar;
pub mod slot;
pub mod update_pill;

pub use host::NavChromeHost;
pub use nav::{
    service_health_body_is_ready, service_health_reason, NavModel, NavOrigin, NavRow, SlotState,
};
pub use pack::pack;
pub use resource_meter::{render_resource_meter, ResourceSnapshot, SVC_BROKER};
pub use sidebar::{icon_letter_for, is_settings_row, render_sidebar, SIDEBAR_WIDTH};
pub use slot::{render_slot, start_service_action, IframeFrame, IframeHealth};
pub use update_pill::{render_changelog_modal, render_update_pill};
