//! WHERE things go — the pluggable layout layer (Build Order §4
//! `graph/layout/`). A *layout backend* turns a [`WorkspaceGraph`] into node
//! positions. C-physics installs the first backend,
//! [`force_directed::ForceDirected`]; C-layout adds `hierarchical.rs` and
//! `stable_grid.rs` alongside it and an animated transition between them.
//!
//! ## Naming: `LayoutBackend`, not `Layout`
//!
//! Build Order §4 labels this trait "Layout trait", but C-scaffold already
//! bound the name [`crate::graph::model::Layout`] to the **positions
//! container** (an id → position map) — the *output* a backend produces.
//! Introducing a second `Layout` (the trait) that the same modules import
//! would be a footgun, so the trait is [`LayoutBackend`] and a backend returns
//! a `model::Layout`. This is a collision-avoidance rename, not a behavioural
//! divergence from the spec.

pub mod config;
pub mod force_directed;

pub use config::LayoutConfig;
pub use force_directed::ForceDirected;

use crate::graph::model::{Layout, WorkspaceGraph};

/// A pluggable layout strategy: graph → positions. Force-directed is iterative
/// (the returned `Layout` is a warm-start the physics worker then refines);
/// the deterministic backends C-layout adds return their final positions
/// straight from [`seed`](LayoutBackend::seed).
pub trait LayoutBackend {
    /// Stable identifier for the backend (used by C-settings / C-layout to
    /// remember the chosen layout per workspace).
    fn name(&self) -> &'static str;

    /// Compute positions for every node in `graph`. For force-directed this is
    /// the warm-start seed; for deterministic backends it is the final layout.
    fn seed(&self, graph: &WorkspaceGraph) -> Layout;
}
