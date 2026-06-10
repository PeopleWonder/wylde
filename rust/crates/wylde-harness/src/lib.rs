//! `wylde-harness` — the chat-brain pipe service.
//!
//! the Wylde user's standing instruction (2026-05-24): the harness is ONE
//! logical thing — one crate, one binary, one pipe — with submodules
//! for the distinct concerns. Phase 5 of the Wylde Rust migration
//! ships the turn-driver submodule; later phases add tooling, memory,
//! and the end-of-turn sweep alongside it.
//!
//! ## Layout
//!
//! * Top-level: shared process-wide utilities (`config`, `state`,
//!   `events`, `service`, `dispatch`).
//! * [`turn`] — the chat-turn driver. Phase 5 of the master plan.
//!   * 5.A (SHIPPED) — `chat.run_turn` non-streaming end-to-end.
//!   * 5.B (THIS SLICE) — `chat.start_turn` / `chat.cancel` /
//!     `chat.stream_turn` / `chat.stream_tools` over the streaming IPC
//!     primitive, with the per-turn registry living in this process.
//!   * 5.C — tool-call decode + dispatch (deferred).
//!   * 5.D — flag-flip default to rust + Python driver deletion.
//!
//! Future phases will add sibling modules (`tooling/`, `memory/`,
//! `end_of_turn/`) — same crate, same binary, same pipe.
//!
//! ## Public entry points
//!
//! * [`service::install`] — register every `chat.*` action on the
//!   shared IPC registry. Idempotent.
//! * [`service::stop`] — drain background workers (5.B's turn-task pool).
//! * [`service::reset_for_tests`] — clear singletons; for tests only.

pub mod api;
// NEW — chat-surface harness modules above the turn driver. Hosts the
// scoped chat-history search tools (Thought Bubble System Slice E).
pub mod chat;
pub mod config;
pub mod dispatch;
pub mod events;
pub mod global_anchors;
pub mod memory;
pub mod model_registry;
pub mod pipe;
// NEW — system-prompt overrides + presets (Rust port of the Python
// `_prompts.py` actions + `Core/shared/system_prompts{,_catalog}.py`;
// full-Rust cutover). Same `data/system_prompts.json`, no migration.
pub mod prompts;
pub mod service;
pub mod settings;
pub mod state;
pub mod tooling;
pub mod turn;
// NEW — global, user-level facts read into every turn (Thought Bubble
// System Slice D). Harness-owned, in-process; the workspace-scoped half
// of the world model lives in `wylde-workspaces`, not here.
pub mod user_profile;
// Workspaces moved out of the harness entirely (Thought Bubble System
// Slice 0d). All workspace-scoped state now lives in the `wylde-workspaces`
// service; the harness reaches it as a pure client via the
// `wylde-workspaces-client` crate (see `turn::workspace_context`).

pub use api::{DefaultHarnessApi, HarnessApi};
pub use service::{install, reset_for_tests, stop};
