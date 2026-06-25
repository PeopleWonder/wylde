//! `wylde-concept-hierarchy` -- the isolated, removable crate that PROJECTS a
//! single definitional-concept DAG from the existing concept / anchor / relation
//! stores (definitional-hierarchy plan, `outputs/definitional-hierarchy-scope.md`).
//!
//! ## What this crate is (H0)
//!
//! The locked design (plan SS0) is a navigable **view / projection** (Option B),
//! not a unified store: one node model `{ id, label, definition, children,
//! parents }` is projected at read time from `concepts.json` + `anchors.json` +
//! `concept_relations.json`. The view IS the unification -- at the API surface the
//! caller sees one [`HierNode`] model -- while every underlying store stays
//! canonical and untouched.
//!
//! **H0 is the pure projection slice and nothing more:**
//!
//! * [`model`] -- the [`HierNode`] / [`HierGraph`] types (the locked shape +
//!   provenance), pure serde, id-linked and flat, never nested on disk.
//! * [`project`] -- [`build_view`], the read-only projection from the three
//!   sources: containment from `parent_concepts` / `parent_anchor`, `names`
//!   cross-references from `described_by`, typed cross-references from the
//!   relation graph; definition by the priority ladder; dangling endpoints
//!   dropped; multi-parent preserved with no duplication.
//! * [`traverse`] -- the in-memory DAG operations: drill-down (children/parents),
//!   the cycle-safe ancestor / descendant walks, the cross-reference walk, and
//!   the definitional [`ancestor_chain`](HierGraph::ancestor_chain) accessor (the
//!   future injection payload).
//! * [`config`] -- the master [`HierarchyConfig`] toggle (default **OFF**,
//!   fail-closed), gating [`build_view_if_enabled`].
//!
//! ## What H0 deliberately is NOT
//!
//! No persistence (the `hierarchy.json` overlay + the id allocator are H1), no
//! verbs / bridge (H1), no GUI sub-tab (H2), no injection (H5), no spread step
//! (H6). Nothing in Core depends on this crate yet -- it is a pure library tested
//! over fixtures, exactly the smallest-first H0 slice.
//!
//! ## Isolation contract (the removal test, plan SS5)
//!
//! No I/O on the projection path, no Core dependency (Core's `Concept` is taken
//! as a pure [`ConceptView`]; only the shared `Anchor` and the sibling routing
//! crate's relation types are used directly), and the toggle defaults OFF so
//! [`build_view_if_enabled`] yields `None` and a toggle-respecting caller sees
//! today's exact behaviour. Deleting this crate restores pre-hierarchy
//! behaviour byte-for-byte.

pub mod config;
pub mod model;
pub mod project;
pub mod traverse;

pub use config::HierarchyConfig;
pub use model::{
    DefSource, Definition, HierGraph, HierNode, NodeId, NodeKind, XRef, XRefKind,
};
pub use project::{build_view, build_view_if_enabled, ConceptView};
pub use traverse::{Reached, WalkOptions};
