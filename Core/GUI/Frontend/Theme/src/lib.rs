//! Wylde visual identity, gpui edition.
//!
//! The Tauri+Svelte tree's source of truth is `Core/GUI/src/app.css`
//! — every CSS custom property in `:root` has a Rust constant here.
//! The intent is that a panel that imports `wylde_theme::colors::*`
//! and `wylde_theme::typography` produces visuals indistinguishable
//! from the Svelte version at first glance.
//!
//! Re-read the plan's §4 for the full translation table.  When app.css
//! changes a token, change the matching constant here in the same
//! commit; the unit tests will spot drift in the value space but not
//! in the token *names*.

pub mod colors;
pub mod typography;
