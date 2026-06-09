//! `anchors/` — the per-workspace anchor store (the world-model layer).
//!
//! **Conceptual path:** `Core/Workspaces/Anchors/`.
//!
//! An **anchor** is the fundamental unit of attention in the Thought Bubble
//! System (Plan v2 §4): a named `{{identifier}}` handle on a code symbol,
//! concept, convention, or person. This module owns the **workspace-scoped**
//! half — anchors saved to one workspace's store. The **global** half lives in
//! the harness (`wylde-harness/src/global_anchors/`); both return identical
//! shapes because the [`Anchor`] type itself lives in [`wylde_shared::anchor`]
//! (see [`anchor`] for the rationale).
//!
//! Slice N-data delivers the full data API; **no UI consumer yet** — Slice N
//! (Phase 4) adds the Vocabulary tab + composer recognition.
//!
//! ## Split
//!
//! * [`anchor`] — re-exports the shared [`Anchor`] model + the workspace-scope
//!   constructor.
//! * [`store`] — `anchors.json` persistence (CRUD + the three lookups).
//! * [`tokenizer`] — `{{identifier}}` parsing (shared) + token→anchor resolve.
//! * [`disambiguation`] — multi-match resolution + the "Anchor this?" seed.
//! * [`promotion`] — the workspace→global 3-context heuristic (Plan v2 §4.4).
//! * [`reflection`] — the LLM-proposed-anchor flow + OI-7 spam control.
//! * [`api`] — the `workspaces.anchors.*` verb handlers.
//!
//! ## Encryption at rest (OI-14)
//!
//! Each `anchors.json` is hardened to owner-only after every write
//! ([`wylde_shared::secure_file::harden_perms`]), the file-level protection the
//! codebase already applies to sensitive state. Full platform-native
//! encryption-at-rest (DPAPI/Keychain/libsecret, Plan v2 §11.4) is a
//! cross-cutting follow-up that should wrap **every** `data_dir` store
//! uniformly rather than anchors alone — flagged in the slice report, not done
//! piecemeal here.

pub mod anchor;
pub mod api;
pub mod disambiguation;
pub mod promotion;
pub mod reflection;
pub mod store;
pub mod tokenizer;

pub use anchor::{Anchor, AnchorKind, AnchorScope, AnchorTarget, SymbolId};
pub use store::CreateOutcome;
