//! `global_anchors/` — the global tier of the anchor system (Slice N-data).
//!
//! **Conceptual path:** `Core/Harness/GlobalAnchors/`.
//!
//! Global anchors are the cross-workspace vocabulary an anchor is **promoted**
//! into (Plan v2 §4.4). This is the harness half of the anchor data layer; the
//! workspace half lives in `wylde-workspaces/src/anchors/`. Both use the same
//! [`Anchor`](wylde_shared::anchor::Anchor) type (hosted in `wylde-shared`, so
//! the wire shapes are byte-identical across the two pipes) and the same
//! atomic-write + owner-only-harden discipline.
//!
//! The harness is otherwise a *pure consumer* of the `wylde-workspaces`
//! service, but **global** anchors are user-level (not workspace-scoped), so
//! they live here alongside `user_profile` and the standalone conversations —
//! the in-process `anchors.*` verbs answer on the harness pipe.
//!
//! ## Split
//!
//! * [`store`] — `global_anchors.json` persistence (CRUD + the three lookups +
//!   the OI-5 collision check + the OI-21 Recommended-Cleanup surface).
//! * [`api`] — the in-process `anchors.*` verb handlers.

pub mod api;
pub mod store;

pub use store::CreateOutcome;
