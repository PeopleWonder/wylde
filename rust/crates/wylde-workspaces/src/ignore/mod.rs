//! Symbol ignore list — workspace + conversation tiers (Slice M, Plan v2
//! §5.8).
//!
//! "Ignore" is *default to inactive from now on*: an ignored token still
//! highlights in the composer and still counts in its chip, but it rides
//! along **deselected** unless the user reactivates it for one message (↺).
//! That's the critical distinction from Exclude (Slice F), which is
//! *this message only*.
//!
//! Three tiers (Build Order §5 / Slice M):
//!   * **conversation** — stored here, keyed by conversation id inside the
//!     workspace's `ignore.json` (a conversation's symbols only mean
//!     anything against its workspace);
//!   * **workspace** — stored here, the flat list in `ignore.json`;
//!   * **global** — lives in the harness (`wylde-harness` `chat/ignore/`),
//!     beside the global anchor store it mirrors.
//!
//! [`store`] owns persistence; [`api`] exposes
//! `workspaces.ignore.{list,add,remove}` (Appendix A: Fast · 500 ms; list =
//! idempotent read, add = idempotent write, remove = no-retry).

pub mod api;
pub mod store;

pub use store::{IgnoreEntry, IgnoreFile, IgnoreTier};
