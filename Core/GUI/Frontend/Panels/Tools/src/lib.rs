//! Tools panel — gpui-era extension manager.
//!
//! Replaces the Svelte alpha's `Core/GUI/src/pages/Tools.svelte`.  Where
//! the Svelte page hosted iframes for each extension's declared UI
//! panel, the gpui Tools panel is a *manager* surface: it lists
//! installed MCP extensions, exposes per-extension enable / disable, and
//! shows which extensions contribute UI panels (so a user can see at a
//! glance what's installed).
//!
//! Why the split?  In the gpui rewrite each declared `ui_panels` entry
//! becomes its own first-class sidebar tab (via the runtime panel
//! overlay — see `wylde-panel-registry::overlay`).  Hosting iframes from
//! *inside* a separate "Tools" tab would mean the sidebar overlay and
//! the Tools tab would each host the same panel, which is confusing.
//! The slot itself handles iframe rendering for each extension panel
//! (see `Core/GUI/Shell/src/slot.rs` `Iframe` branch); the Tools panel
//! focuses on the management surface.
//!
//! Slice 4 scope:
//!   - List extensions via `ext.list` (id, version, enabled, status).
//!   - Toggle enable / disable via `ext.enable` / `ext.disable`.
//!   - List declared panels via `extensions.list_panels` so the user
//!     can see which extensions contribute panels without navigating to
//!     each one.
//!
//! Out of scope (later slices):
//!   - Live `ext.events` streaming subscription.
//!   - Per-extension capability inspection (tools.list per extension).
//!   - Install / uninstall flows.
//!
//! The Svelte original stays the source of truth during the alpha; we
//! do not touch it.  Cutover deletes `src-tauri/` + `src/` together.

pub mod ipc;
pub mod tools_panel;

pub use tools_panel::ToolsPanel;
