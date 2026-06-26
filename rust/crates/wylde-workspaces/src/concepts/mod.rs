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
pub mod freshness;
pub mod identity;
// Concept-routing R2 — the deletable Augment-injection seam: boundary blurb
// (from relations) + member snippets for the user-curated concepts. Remove with
// the crate to revert to pre-injection routing.
pub mod inject;
pub mod lens;
pub mod proposals;
pub mod retrieve;
// Concept-routing plan R0/R1 — the deletable seam to the isolated
// `wylde-concept-routing` crate (load store + match vocab + call the pure
// router with the shared RAG embed). Remove with the crate to revert routing.
pub mod routing_bridge;
// Concept-routing R1.5a — the deletable typed-relation store + the
// `workspaces.concepts.relations.*` verbs (sibling of `routing_bridge`).
// Remove with the crate to revert to pure-seed R1 (empty graph = identity).
pub mod relations_bridge;
// Definitional concept hierarchy H1 — the deletable overlay store + the
// `workspaces.hierarchy.*` verbs (sibling of `relations_bridge`). Maps the
// Core `Concept` into the isolated crate's `ConceptView`, persists the additive
// `hierarchy.json` overlay + `hierarchy_identity.json` allocator, and folds the
// overlay onto the projected view. Remove with the crate + the overlay store to
// revert to today (an empty overlay is the projection's identity).
pub mod hierarchy_bridge;
pub mod search;
pub mod semantic;
pub mod store;

pub use concept::{Concept, ConceptSource};
