//! `user_profile/` — global, user-level facts the assistant reads every
//! turn. Thought Bubble System **Slice D** (Phase 2 — harness layer).
//!
//! This is the harness's half of "the AI gets smarter": a small,
//! editable [`UserProfile`](profile::UserProfile) (name, preferences,
//! recurring topics, style, free-text rules) persisted at
//! `<data_dir>/user_profile.json`, surfaced as the Settings
//! "Profile / Rules" page, and offered LLM-proposed updates the user
//! accepts / edits / rejects.
//!
//! Greenfield Rust — there is no Python predecessor and no strangler
//! gate (the workspaces redesign and Slice 0d left the harness
//! workspace-free; this module is brand-new harness-owned state).
//!
//! ## Submodules (Build Order §3)
//!
//! * [`profile`] — the [`UserProfile`](profile::UserProfile) data model,
//!   the user-edit patch path (OI-18), and the LLM-proposal value types.
//! * [`store`] — JSON persistence at `<data_dir>/user_profile.json`
//!   (owner-only; encryption-at-rest is an OI-14 follow-up — see the
//!   module docs).
//! * [`reflection`] — the spam-controlled proposal flow (OI-7 / OI-11)
//!   and the post-turn reflection trigger scaffold.
//! * [`api`] — the `user_profile.{get, update, propose, accept, reject,
//!   list_proposals}` action handlers.
//!
//! ## Verbs (Build Order Appendix A)
//!
//! All `user_profile.*` verbs are **in-process** harness verbs — served
//! directly out of [`store`], with no `wylde-workspaces` pipe hop and so
//! no `wylde-workspaces-client` timeout/retry/cache tier. The verb
//! surface is registered in [`crate::pipe`] and forwarded from
//! [`crate::api::HarnessApi`]. (`list_proposals` extends the plan's
//! five-verb surface so the Settings UI can render the pending queue.)

pub mod api;
pub mod profile;
pub mod reflection;
pub mod store;

#[cfg(test)]
pub mod test_support;

pub use profile::UserProfile;
