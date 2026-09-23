//! The host trait the nav-chrome renderers dispatch to.
//!
//! The renderers ([`sidebar`](crate::sidebar), [`slot`](crate::slot),
//! [`update_pill`](crate::update_pill)) used to take `Context<Shell>` and wire
//! `cx.listener(|this: &mut Shell, …|)` handlers straight to the concrete
//! `Shell` — which lives in the `wry`/tray-linking `wylde-gui` crate, so the
//! headless L7 panel-walk job could not build them (#247). Making them generic
//! over this small trait severs that: the Shell `impl NavChromeHost for Shell`
//! delegating to its existing methods, and a walk fixture implements a fake
//! host with trivial handlers — no `wry`, no tray, no webview.
//!
//! `Render + 'static` because the renderers run inside the host view's
//! `render` and their `cx.listener`s dispatch back through its `Entity` handle.

use std::sync::Arc;

use gpui::{Context, Render};

/// What the Shell's nav chrome needs its host to be able to do. Mirrors the
/// six `Shell` methods the sidebar / slot / update-pill click handlers call.
pub trait NavChromeHost: Render + 'static {
    /// Select the panel keyed `key`; returns whether the key was known.
    fn on_nav_click(&mut self, key: &str) -> bool;
    /// Start the stopped service the slot's recovery affordance names.
    fn on_start_service_click(&mut self, service: Arc<str>, cx: &mut Context<Self>);
    /// Open the changelog overlay (the update pill's "What's new").
    fn open_changelog(&mut self, cx: &mut Context<Self>);
    /// Act on the available update (the update pill's "Update").
    fn on_update_click(&mut self, cx: &mut Context<Self>);
    /// Dismiss the offered `version` (the update pill's "Ignore").
    fn on_ignore_click(&mut self, version: String, cx: &mut Context<Self>);
    /// Close the changelog overlay (its ✕ / scrim).
    fn close_changelog(&mut self, cx: &mut Context<Self>);
}
