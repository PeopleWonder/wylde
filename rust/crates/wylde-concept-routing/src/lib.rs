//! `wylde-concept-routing` — the isolated, removable decision layer that
//! routes a chat turn's query into *concept space* (concept-routing plan,
//! `outputs/concept-routing-plan.md`).
//!
//! ## What this crate is
//!
//! The concept **mechanism** (centroids, lens, concept-driven retrieval,
//! hybrid search, curation, freshness) already shipped in TBS Phases 0–4 and
//! lives in `wylde-workspaces`. This crate is *only* the **selection policy**:
//! given a turn's embedded query and the workspace's concept centroids, decide
//! *which* concepts to activate — plus the four-requirement wrapper (master
//! toggle, curate-before-inject, dependency tree, separated folder) and the
//! eval that proves the thesis claim.
//!
//! ## Phase status (build order, plan §7)
//!
//! * **R0 (this slice)** — [`config`]: the master [`RoutingConfig`] toggle
//!   (default **off** ⇒ today's exact RAG). Pure, fail-soft, harness-owned.
//! * **R1 (this slice)** — [`router`]: route-and-**log**. Cosine the query
//!   against centroids, select with the `dynamic_k`-shaped cutoff, match
//!   vocabulary, and produce a [`CandidateSet`]. **No injection** — the
//!   `CandidateSet` is logged as threshold-calibration data and discarded.
//! * **R2 (this slice)** — [`curation`]: the curate-before-inject menu
//!   ([`CuratedMenu`]) + the apply step ([`apply_curation`] →
//!   [`InjectionPlan`], token-budget eviction). The server-side bridge turns the
//!   plan into the boundary blurb + member snippets (Augment injection).
//! * **R3a (this slice)** — [`lens_select`]: the scoped-lens region derivation
//!   ([`region_for_active_file`]) that narrows a curated concept's injection to
//!   the active file's subsystem (`concepts/lens.rs` does the intersection).
//! * **R4** — [`eval`]: stub today; filled in by the final slice (the eval arms).
//!
//! ## Isolation contract (the removal test, plan §2)
//!
//! With [`RoutingConfig::enabled`] compiled to `false` the routing path is
//! never entered, so deleting this crate from the workspace + reverting the
//! two gated seams must leave Core building and behaving **byte-identically**
//! to pre-routing. Nothing here reads global mutable state except the config
//! cache (the `privacy_prefs` `OnceLock<Mutex>` shape), and the routing math
//! is pure.

pub mod config;
pub mod curation;
pub mod eval;
pub mod lens_select;
pub mod relations;
pub mod router;

pub use config::{InjectionMode, RelationParams, RoutingConfig};
pub use lens_select::region_for_active_file;
pub use curation::{
    apply_curation, CuratedMenu, InjectionPlan, MenuAnnotation, MenuItem, MenuItemKind,
};
pub use relations::{NodeRef, Relation, RelationGraph, RelationKind};
pub use router::spread::{spread, Provenance, SpreadResult};
pub use router::{route, CandidateSet, ConceptCentroid, RoutedConcept, VocabMatch};
