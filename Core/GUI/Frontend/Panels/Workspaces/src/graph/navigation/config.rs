//! Navigation behaviour knobs (Slice C-navigation).
//!
//! Per the Build Order §8 convention ("every tunable lives in exactly one
//! `config.rs`"): everything here is a **behavioural** knob Aaron can tweak in
//! one place during the feel-test. **Visual** values (colours, sizes,
//! durations, easings) are NOT here — they come from the [`Theme`]
//! (`graph_panel.breadcrumb_bar`, `graph_panel.exit_edges`,
//! `animations.graph_zoom_into_cluster` / `graph_zoom_out`).
//!
//! [`Theme`]: crate::graph::render::Theme

use serde::{Deserialize, Serialize};

/// Tunables for the space-map navigation feel. Serializable so a settings
/// profile (C-settings `graph_profiles.json`) snapshots it verbatim;
/// `#[serde(default)]` keeps older profiles loading as knobs are added.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct NavConfig {
    /// Zoom multiplier per scroll unit (one wheel notch = one unit). The
    /// scaffold camera hardcoded 1.15; it now lives here.
    pub zoom_step_factor: f32,
    /// A scope exits when the camera zoom drops below
    /// `min(zoom_threshold, entry_fit_zoom) × leave_hysteresis`. Below 1.0 so
    /// hovering right at the threshold can't flap enter/leave.
    pub leave_hysteresis: f32,
    /// Fraction of the canvas the entered cluster's bounds fill after the
    /// zoom-in tween (matches the first-load fit margin).
    pub cluster_fit_margin: f32,
    /// How many solid segments approximate one exit-edge fade stub (gpui has
    /// no per-vertex alpha; more segments = smoother fade, more draw calls).
    pub exit_stub_segments: usize,
    /// Cap on distinct exit-edge destination labels rendered at once (clutter
    /// + element-count guard; stubs themselves are not capped).
    pub max_exit_labels: usize,
}

impl Default for NavConfig {
    fn default() -> Self {
        NavConfig {
            zoom_step_factor: 1.15,
            leave_hysteresis: 0.8,
            cluster_fit_margin: 0.85,
            exit_stub_segments: 6,
            max_exit_labels: 12,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = NavConfig::default();
        assert!(c.zoom_step_factor > 1.0, "scroll-in zooms in");
        assert!(
            (0.0..1.0).contains(&c.leave_hysteresis),
            "hysteresis below 1 prevents enter/leave flapping"
        );
        assert!((0.0..=1.0).contains(&c.cluster_fit_margin));
        assert!(c.exit_stub_segments >= 2, "at least a visible fade");
        assert!(c.max_exit_labels > 0);
    }
}
