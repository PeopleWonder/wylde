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
//! * **R2+** — [`curation`], [`lens_select`], [`eval`]: stubs today; filled in
//!   by later slices (curate-before-inject menu, scoped lens, the eval arms).
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
pub use relations::{NodeRef, Relation, RelationGraph, RelationKind};
pub use router::spread::{spread, Provenance, SpreadResult};
pub use router::{route, CandidateSet, ConceptCentroid, RoutedConcept, VocabMatch};
