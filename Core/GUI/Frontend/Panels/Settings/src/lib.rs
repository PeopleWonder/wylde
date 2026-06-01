//! Settings panel — gpui-era port of
//! `Core/GUI/src/pages/Settings.svelte`.
//!
//! Scoped to the slice 2 spec:
//!   - Updates section (master toggle + auto-check + frequency picker
//!     + manual-check button) — Phase 12.5 surface.
//!   - Startup section (autostart toggle via `auto-launch`) —
//!     Phase 12.3, default off.
//!   - Ollama inference defaults — small numeric form.
//!   - Consent section — global "no-auth" toggle + per-tool list,
//!     backed by the Phase 12.2 + 12.6 `consent.*` pipe verbs.
//!
//! Sections explicitly out of scope (next slices):
//!   - System Prompts editor + presets.
//!   - n8n credentials.
//!   - Hardware re-scan card.
//!   - Memory + Voice sub-settings (their own Frontend slices).
//!
//! The Svelte original at `Core/GUI/src/pages/Settings.svelte` stays
//! the source of truth for the alpha — this crate runs side-by-side
//! through the gpui rewrite cutover.

pub mod ipc;
pub mod sections;
pub mod settings_panel;

pub use settings_panel::SettingsPanel;
