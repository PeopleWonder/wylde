//! Space-map navigation — WHERE the user is looking (Build Order §4).
//!
//! Created in the 2026-06-09 pre-C-navigation cleanup: [`input`] holds the
//! pointer/keyboard handlers moved out of `graph/mod.rs` (no behaviour
//! change). Slice C-navigation grows this module to the full spec:
//!
//!   * `camera.rs` — scope-aware camera (zoom, pan, current cluster path)
//!     wrapping the projection primitive in `render::viewport::Camera`.
//!   * `breadcrumb.rs` — the breadcrumb bar (Theme `graph_panel.breadcrumb_bar`).
//!   * `transition.rs` — camera tweens (Theme `graph_zoom_into_cluster` /
//!     `graph_zoom_out`); distinct from the layout-swap driver in
//!     `graph/transition_driver.rs`.
//!   * `input.rs` — grows scroll/click/key → `NavAction` translation
//!     (zoom-toward-cursor, threshold enter/leave, exit-edge clicks).

pub mod input;
