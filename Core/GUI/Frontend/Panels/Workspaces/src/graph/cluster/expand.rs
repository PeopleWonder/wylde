//! Expand-in-place mechanics (Slice C-cluster, Build Order §4 `expand.rs`).
//!
//! A folded cluster sphere expands **in place**: its members animate out
//! from the cluster centroid to their layout positions over the Theme's
//! `cluster_expand_in_place` animation (300 ms, ease-out); collapsing runs
//! the same path in reverse. Pure + time-injected (elapsed ms in, positions
//! out) so tests step it deterministically; the GraphView drives it from the
//! same ~60 fps main-thread pattern as the other tweens.
//!
//! Fold state itself is resolved here too: a cluster is folded when
//! auto-clustering selected it AND the camera hasn't zoomed past its
//! `zoom_threshold` — unless the user overrode it (right-click → Expand /
//! Collapse Cluster). Zoom crossings clear stale overrides so the space-map
//! keeps feeling zoom-driven.

use std::collections::{HashMap, HashSet};

use crate::graph::layout::CubicBezier;
use crate::graph::model::Position;

/// The locked `cluster_expand_in_place` spec (Visual Style v1) — degrade
/// fallback only; the live values come FROM the Theme.
pub const EXPAND_FALLBACK_MS: f32 = 300.0;
pub const EXPAND_FALLBACK_EASING: CubicBezier = CubicBezier::new(0.16, 1.0, 0.3, 1.0);

/// One in-flight expand or collapse: a tween over **expansion progress**
/// (0 = fully folded, members at the centroid; 1 = fully expanded, members at
/// their layout positions). `from` is wherever the cluster currently is, so a
/// reversal mid-animation (zoom jiggling across a threshold) retargets
/// smoothly instead of snapping.
#[derive(Clone, Copy, Debug)]
pub struct ExpandAnim {
    pub from: f32,
    pub to: f32,
    pub duration_ms: f32,
    pub easing: CubicBezier,
}

impl ExpandAnim {
    /// Expansion progress at `elapsed_ms`.
    pub fn progress(&self, elapsed_ms: f32) -> f32 {
        let raw = (elapsed_ms / self.duration_ms.max(1.0)).clamp(0.0, 1.0);
        self.from + (self.to - self.from) * self.easing.ease(raw)
    }

    pub fn is_done(&self, elapsed_ms: f32) -> bool {
        elapsed_ms >= self.duration_ms
    }

    pub fn is_expanding(&self) -> bool {
        self.to > self.from
    }
}

/// Interpolate a member's render position for expansion `progress`.
pub fn member_position(centroid: Position, target: Position, progress: f32) -> Position {
    let p = progress.clamp(0.0, 1.0);
    Position {
        x: centroid.x + (target.x - centroid.x) * p,
        y: centroid.y + (target.y - centroid.y) * p,
        z: 0.0,
    }
}

/// A user override from the right-click menu.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Override {
    Expanded,
    Collapsed,
}

/// Resolve the desired fold set for the current frame: start from the
/// auto-fold selection, unfold anything the camera zoomed past
/// (`zoom ≥ zoom_threshold` — the C-navigation "clusters unfold as you
/// approach" rule), then apply user overrides on top.
pub fn desired_folds(
    auto_folds: &HashSet<String>,
    thresholds: &HashMap<String, f32>,
    zoom: f32,
    overrides: &HashMap<String, Override>,
) -> HashSet<String> {
    let mut folds: HashSet<String> = auto_folds
        .iter()
        .filter(|id| {
            let t = thresholds.get(*id).copied().unwrap_or(1.0);
            zoom < t // zoomed past the threshold → unfolds
        })
        .cloned()
        .collect();
    for (id, ov) in overrides {
        match ov {
            Override::Expanded => {
                folds.remove(id);
            }
            Override::Collapsed => {
                // Collapse override only makes sense for clusters the
                // auto-selector considers foldable at all.
                if auto_folds.contains(id) {
                    folds.insert(id.clone());
                }
            }
        }
    }
    folds
}

/// The desired fold set with the clusters-first tier on top (visual-polish
/// G1). At or below `clusters_first_zoom`, **every** known cluster
/// (`thresholds` carries one entry per cluster) folds to a sphere — the
/// galaxy view — except clusters the user explicitly expanded. Above it, this
/// is exactly [`desired_folds`] (auto-fold selection + per-cluster zoom
/// thresholds + overrides).
pub fn desired_folds_at_zoom(
    auto_folds: &HashSet<String>,
    thresholds: &HashMap<String, f32>,
    zoom: f32,
    overrides: &HashMap<String, Override>,
    clusters_first_zoom: f32,
) -> HashSet<String> {
    if zoom <= clusters_first_zoom {
        let mut folds: HashSet<String> = thresholds.keys().cloned().collect();
        // A user who explicitly expanded a sphere keeps it open even in the
        // galaxy view; a Collapsed override is moot (already folded).
        for (id, ov) in overrides {
            if *ov == Override::Expanded {
                folds.remove(id);
            }
        }
        return folds;
    }
    desired_folds(auto_folds, thresholds, zoom, overrides)
}

/// Drop overrides the zoom has caught up with (an "Expand Cluster" override
/// is satisfied once the zoom unfolds it anyway, and vice versa), so manual
/// choices don't permanently pin clusters against the space-map's
/// zoom-driven feel.
pub fn prune_overrides(
    overrides: &mut HashMap<String, Override>,
    thresholds: &HashMap<String, f32>,
    zoom: f32,
) {
    overrides.retain(|id, ov| {
        let t = thresholds.get(id).copied().unwrap_or(1.0);
        let zoom_unfolds = zoom >= t;
        match ov {
            Override::Expanded => !zoom_unfolds,
            Override::Collapsed => zoom_unfolds,
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn anim(from: f32, to: f32) -> ExpandAnim {
        ExpandAnim {
            from,
            to,
            duration_ms: 300.0,
            easing: EXPAND_FALLBACK_EASING,
        }
    }

    #[test]
    fn expand_progress_runs_zero_to_one() {
        let a = anim(0.0, 1.0);
        assert!(a.is_expanding());
        assert_eq!(a.progress(0.0), 0.0);
        let mid = a.progress(150.0);
        assert!(mid > 0.0 && mid < 1.0);
        assert_eq!(a.progress(300.0), 1.0);
        assert!(a.is_done(300.0) && !a.is_done(299.0));
    }

    #[test]
    fn collapse_progress_runs_one_to_zero() {
        let a = anim(1.0, 0.0);
        assert!(!a.is_expanding());
        assert_eq!(a.progress(0.0), 1.0);
        assert_eq!(a.progress(300.0), 0.0);
    }

    #[test]
    fn mid_flight_reversal_starts_from_current_progress() {
        // Expanding reached 0.6 when the user zoomed back out: the collapse
        // tween starts at 0.6, not a snap to 1.0.
        let reversed = anim(0.6, 0.0);
        assert_eq!(reversed.progress(0.0), 0.6);
        assert_eq!(reversed.progress(300.0), 0.0);
        let mid = reversed.progress(150.0);
        assert!(mid < 0.6 && mid > 0.0);
    }

    #[test]
    fn member_position_lerps_centroid_to_target() {
        let c = Position {
            x: 0.0,
            y: 0.0,
            z: 0.0,
        };
        let t = Position {
            x: 100.0,
            y: -40.0,
            z: 0.0,
        };
        let at0 = member_position(c, t, 0.0);
        assert_eq!((at0.x, at0.y), (0.0, 0.0));
        let at1 = member_position(c, t, 1.0);
        assert_eq!((at1.x, at1.y), (100.0, -40.0));
        let mid = member_position(c, t, 0.5);
        assert_eq!((mid.x, mid.y), (50.0, -20.0));
    }

    fn setup() -> (HashSet<String>, HashMap<String, f32>) {
        let auto: HashSet<String> = ["a", "b"].iter().map(|s| (*s).to_owned()).collect();
        let mut th = HashMap::new();
        th.insert("a".to_owned(), 1.0);
        th.insert("b".to_owned(), 2.0);
        (auto, th)
    }

    #[test]
    fn zoom_unfolds_past_threshold() {
        let (auto, th) = setup();
        let none = HashMap::new();
        // Low zoom: everything auto-selected stays folded.
        let f = desired_folds(&auto, &th, 0.5, &none);
        assert!(f.contains("a") && f.contains("b"));
        // Past a's threshold but not b's.
        let f = desired_folds(&auto, &th, 1.5, &none);
        assert!(!f.contains("a") && f.contains("b"));
        // Past both.
        let f = desired_folds(&auto, &th, 2.5, &none);
        assert!(f.is_empty());
    }

    #[test]
    fn overrides_beat_zoom_state() {
        let (auto, th) = setup();
        let mut ov = HashMap::new();
        ov.insert("a".to_owned(), Override::Expanded);
        let f = desired_folds(&auto, &th, 0.5, &ov);
        assert!(!f.contains("a"), "expand override unfolds at low zoom");
        assert!(f.contains("b"));

        let mut ov = HashMap::new();
        ov.insert("a".to_owned(), Override::Collapsed);
        let f = desired_folds(&auto, &th, 1.5, &ov);
        assert!(f.contains("a"), "collapse override re-folds past threshold");
    }

    #[test]
    fn collapse_override_ignored_for_non_auto_clusters() {
        let (auto, th) = setup();
        let mut ov = HashMap::new();
        ov.insert("not-auto".to_owned(), Override::Collapsed);
        let f = desired_folds(&auto, &th, 0.5, &ov);
        assert!(!f.contains("not-auto"));
    }

    #[test]
    fn clusters_first_folds_everything_below_threshold() {
        let (auto, th) = setup(); // auto = {a,b}; th has a,b
        let none = HashMap::new();
        // Above the clusters-first zoom → ordinary per-cluster logic.
        let f = desired_folds_at_zoom(&auto, &th, 1.5, &none, 0.35);
        assert!(!f.contains("a") && f.contains("b"), "ordinary tier above threshold");
        // At/below it → every cluster in `thresholds` folds (the galaxy view),
        // even ones the auto-selector wouldn't have folded.
        let mut th2 = th.clone();
        th2.insert("c".to_owned(), 5.0); // a cluster NOT in auto_folds
        let f = desired_folds_at_zoom(&auto, &th2, 0.2, &none, 0.35);
        assert!(f.contains("a") && f.contains("b") && f.contains("c"));
        // An explicit Expanded override stays open even in the galaxy view.
        let mut ov = HashMap::new();
        ov.insert("a".to_owned(), Override::Expanded);
        let f = desired_folds_at_zoom(&auto, &th2, 0.2, &ov, 0.35);
        assert!(!f.contains("a") && f.contains("b") && f.contains("c"));
    }

    #[test]
    fn prune_drops_satisfied_overrides() {
        let (_, th) = setup();
        let mut ov = HashMap::new();
        ov.insert("a".to_owned(), Override::Expanded); // threshold 1.0
        ov.insert("b".to_owned(), Override::Collapsed); // threshold 2.0

        // zoom 1.5: a's expand is satisfied by zoom (≥1.0) → dropped;
        // b's collapse fights zoom-folded state (zoom < 2.0 folds it anyway) → dropped.
        prune_overrides(&mut ov, &th, 1.5);
        assert!(!ov.contains_key("a"));
        assert!(!ov.contains_key("b"));

        // Re-add at a zoom where each override still does work → retained.
        ov.insert("a".to_owned(), Override::Expanded);
        prune_overrides(&mut ov, &th, 0.5); // zoom below threshold: expand matters
        assert!(ov.contains_key("a"));
        ov.insert("b".to_owned(), Override::Collapsed);
        prune_overrides(&mut ov, &th, 2.5); // zoom above: collapse matters
        assert!(ov.contains_key("b"));
    }
}
