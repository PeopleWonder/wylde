//! Shared anchor edit affordances (Slice N, Plan v2 §6).
//!
//! **One source of truth for edit affordances.** Bubbles in the chat, nodes
//! in the graph view, and entries in the Vocabulary tab are three lenses on
//! the same anchor data; the *semantics* of editing it live here exactly
//! once:
//!
//!   * [`exclude_ignore`] — the Exclude (this-message) vs Ignore
//!     (default-inactive) state machine (Plan §5.8).
//!   * [`connection_edit`] — the add/remove-connection flow (drawing mode +
//!     peer-to-peer card, OI-22) as a pure draft/validate/commit model.
//!   * [`approval_gate`] — confirmation gating for permanent changes.
//!   * [`undo`] — the per-conversation undo/redo stack (Plan §5.9, depth 50).
//!   * [`menus`] — the shared right-click menu *definition* (consumed by
//!     bubbles AND graph nodes AND vocabulary rows; each renders it with
//!     its own chrome).
//!
//! gpui-free by design — consumers own rendering and IPC. The chat
//! composer's Slice F/M word-state predates this crate and is behaviourally
//! aligned (its tests pin the same §5.8 transitions); migrating it onto
//! these types is a mechanical cleanup queued for the bubble-layer pass.

pub mod approval_gate;
pub mod connection_edit;
pub mod exclude_ignore;
pub mod menus;
pub mod undo;

pub use approval_gate::ApprovalGate;
pub use connection_edit::{ConnectionDraft, ConnectionError};
pub use exclude_ignore::{Activation, IgnoreTier};
pub use menus::{anchor_menu, MenuAction, MenuContext};
pub use undo::UndoStack;
