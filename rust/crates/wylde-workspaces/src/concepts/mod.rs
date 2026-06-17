//! `concepts/` — the per-workspace **Concepts** layer of the Thought Bubble
//! System's three-layer semantic map (thesis `outputs/wylde-tbs-concept-system-thesis.md`).
//!
//! A concept is a *system-discovered* semantic theme over the code graph — the
//! complement to a user-curated [`crate::anchors`] anchor (the Vocabulary
//! layer). This module owns the concept store, its read/write/curate verbs, and
//! the Phase-0 cheap-concept builder.
//!
//! ## Split (mirrors `anchors/`)
//!
//! * [`concept`] — the [`Concept`] data model (member-set + centroid +
//!   description; DAG parents; provenance).
//! * [`store`] — `concepts.json` persistence (CRUD + reverse-lookup queries),
//!   encrypted-at-rest, fail-soft. **Authoritative** — the graph projection is
//!   an additive sync, never the read path.
//! * [`cheap`] — Phase-0 stand-in: label `cluster_by_dir` directory clusters
//!   into concepts (proves the pipeline before semantic clustering, thesis §7).
//! * [`api`] — the `workspaces.concepts.*` verb handlers.
//!
//! Phase upgrade path (thesis §7): Phase 2 swaps the concept *source* from
//! [`cheap`] directory clusters to embedding clusters of the chunk vectors,
//! filling [`Concept::centroid`], without changing the store or the browse
//! surface built on it.

pub mod api;
pub mod cheap;
pub mod clustering;
pub mod concept;
pub mod lens;
pub mod proposals;
pub mod retrieve;
pub mod search;
pub mod semantic;
pub mod store;

pub use concept::{Concept, ConceptSource};
