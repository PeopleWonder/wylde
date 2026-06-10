//! Graph settings — global profiles + per-workspace bookmark (Slice
//! C-settings, Plan v2 §10 / Build Order §4).
//!
//!   * [`profiles`]          — [`GraphProfile`] (a full `GraphSettings` +
//!     `ThemeSettings` + `InteractionSettings` snapshot, Appendix B) and the
//!     [`ProfileLibrary`].
//!   * [`persistence`]       — load/save from `<data_dir>/graph_profiles.json`
//!     (atomic writes; missing/corrupt → defaults).
//!   * [`workspace_pointer`] — the per-workspace `last_profile` bookmark
//!     (a string per workspace, nothing more — Plan §10).
//!
//! Applying a profile re-points the live knob structs (`LayoutKind`,
//! [`ClusterConfig`], [`NavConfig`], dark mode) and runs the Theme's
//! `graph_profile_switch` camera tween (500 ms) into the new view — see
//! `GraphView::apply_profile`.
//!
//! [`ClusterConfig`]: crate::graph::cluster::ClusterConfig
//! [`NavConfig`]: crate::graph::navigation::NavConfig

pub mod persistence;
pub mod profiles;
pub mod workspace_pointer;

pub use profiles::{
    GraphProfile, GraphSettings, InteractionSettings, ProfileLibrary, ThemeSettings,
    DEFAULT_PROFILE,
};
