//! Space-map navigation — WHERE the user is looking (Build Order §4).
//!
//! Slice C-navigation: scroll-zoom **enters** a cluster when the zoom crosses
//! its `zoom_threshold` while the cursor is over it; zooming back out
//! **leaves** (with hysteresis so the boundary doesn't flap). The scope trail
//! renders as a breadcrumb bar; edges that leave the scoped cluster fade to
//! "exit edges" with destination labels — clicking one zooms out and re-zooms
//! into the target cluster.
//!
//!   * [`camera`]     — zoom-toward-cursor + fit math (pure).
//!   * [`config`]     — behavioural knobs ([`NavConfig`]); visual values stay
//!     in the Theme.
//!   * [`transition`] — camera tweens (Theme `graph_zoom_into_cluster` /
//!     `graph_zoom_out`); distinct from the layout-swap driver in
//!     `graph/transition_driver.rs`.
//!   * [`breadcrumb`] — scope-trail model + the gpui bar (Theme
//!     `graph_panel.breadcrumb_bar`).
//!   * [`input`]      — pointer/keyboard handlers (scroll → zoom + threshold
//!     checks, clicks, drags).
//!
//! [`Navigator`] owns the scope stack. v1 wire clusters (Slice B: one per
//! file parent directory) are flat, so the stack is effectively depth ≤ 1
//! until C-cluster lands real hierarchy — the stack shape is already
//! hierarchical so C-cluster only changes who pushes onto it.

pub mod breadcrumb;
pub mod camera;
pub mod config;
pub mod input;
pub mod transition;

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use crate::graph::model::{Cluster, Layout, WorkspaceGraph};
use crate::graph::render::viewport::Camera;

pub use config::NavConfig;

/// One scope level: which cluster the user is inside and how to get back out.
#[derive(Clone, Debug)]
pub struct ScopeEntry {
    pub cluster_id: String,
    /// The camera as it was just before entering — the zoom-out tween's
    /// target, so leaving puts you back where you were.
    pub saved_camera: Camera,
    /// Zoom below which this scope exits:
    /// `min(zoom_threshold, entry_fit_zoom) × leave_hysteresis`. The fit-zoom
    /// term guards a large cluster whose framed zoom sits *below* its own
    /// threshold from exiting on the first scroll.
    pub leave_below: f32,
}

/// What an input gesture asks the navigation to do.
#[derive(Clone, Debug, PartialEq)]
pub enum NavAction {
    /// Zoom crossed a cluster's threshold while the cursor was over it.
    EnterCluster(String),
    /// Zoom dropped below the active scope's leave point (or the root crumb
    /// was clicked).
    LeaveScope,
    /// An exit-edge label was clicked: zoom out, then re-zoom into the target.
    JumpToCluster(String),
}

/// Owns the scope stack + navigation knobs. Pure state — camera moves are the
/// view's job (it arms tweens off the [`NavAction`]s this returns).
#[derive(Default)]
pub struct Navigator {
    pub config: NavConfig,
    stack: Vec<ScopeEntry>,
    /// Member-id set of the innermost scope, shared with the render path
    /// (`Rc` so the paint closure clones a handle, not the set).
    members: Option<Rc<HashSet<String>>>,
}

impl Navigator {
    pub fn is_scoped(&self) -> bool {
        !self.stack.is_empty()
    }

    pub fn scope(&self) -> Option<&ScopeEntry> {
        self.stack.last()
    }

    pub fn depth(&self) -> usize {
        self.stack.len()
    }

    /// The innermost scope's member set (render-path filter), if scoped.
    pub fn members(&self) -> Option<Rc<HashSet<String>>> {
        self.members.clone()
    }

    /// Decide whether a zoom change enters or leaves a scope.
    ///
    /// Enter: not scoped, zooming **in**, and the zoom **crossed** a cluster's
    /// threshold (`old < t ≤ new`) while the cursor's model point sat inside
    /// that cluster's bounds. Overlapping candidates → smallest bounds wins
    /// (the most specific cluster). Leave: scoped and the zoom fell below the
    /// scope's `leave_below`. Nested enters are C-cluster's job.
    pub fn action_for_zoom(
        &self,
        old_zoom: f32,
        new_zoom: f32,
        cursor_model: (f32, f32),
        graph: &WorkspaceGraph,
        layout: &Layout,
    ) -> Option<NavAction> {
        if let Some(top) = self.stack.last() {
            if new_zoom < top.leave_below {
                return Some(NavAction::LeaveScope);
            }
            return None;
        }
        if new_zoom <= old_zoom {
            return None;
        }
        let mut best: Option<(&Cluster, f32)> = None;
        for c in &graph.clusters {
            if !(old_zoom < c.zoom_threshold && new_zoom >= c.zoom_threshold) {
                continue;
            }
            let Some(bb) = camera::members_bounds(&c.member_ids, layout, 0.0) else {
                continue;
            };
            if !camera::bounds_contain(bb, cursor_model.0, cursor_model.1) {
                continue;
            }
            let area = camera::bounds_area(bb);
            if best.is_none_or(|(_, a)| area < a) {
                best = Some((c, area));
            }
        }
        best.map(|(c, _)| NavAction::EnterCluster(c.id.clone()))
    }

    /// Push `cluster` onto the scope stack. `saved_camera` is restored on
    /// leave; `fit_zoom` is the zoom the enter tween lands on (feeds the
    /// `leave_below` guard).
    pub fn enter(&mut self, cluster: &Cluster, saved_camera: Camera, fit_zoom: f32) {
        let leave_below = cluster.zoom_threshold.min(fit_zoom) * self.config.leave_hysteresis;
        self.stack.push(ScopeEntry {
            cluster_id: cluster.id.clone(),
            saved_camera,
            leave_below,
        });
        self.members = Some(Rc::new(cluster.member_ids.iter().cloned().collect()));
    }

    /// Pop the innermost scope, returning it (its `saved_camera` is the
    /// zoom-out tween target). Rebuilds the member filter for the scope that
    /// remains, if any.
    pub fn leave(&mut self, graph: &WorkspaceGraph) -> Option<ScopeEntry> {
        let popped = self.stack.pop();
        self.members = self
            .stack
            .last()
            .and_then(|e| graph.cluster_by_id(&e.cluster_id))
            .map(|c| Rc::new(c.member_ids.iter().cloned().collect()));
        popped
    }

    /// Drop all scope state (graph reload / workspace switch).
    pub fn reset(&mut self) {
        self.stack.clear();
        self.members = None;
    }
}

// ── Exit edges ───────────────────────────────────────────────────────────

/// One fading stub: an edge that leaves the scope, truncated at the Theme's
/// `fade_distance_px` from its in-scope endpoint, pointing at the (hidden)
/// external endpoint. Window-px coordinates; `rel_key` picks the edge's
/// Theme colour.
#[derive(Clone, Debug, PartialEq)]
pub struct ExitStub {
    pub x0: f32,
    pub y0: f32,
    pub x1: f32,
    pub y1: f32,
    pub rel_key: &'static str,
    pub thickness: f32,
}

/// One destination chip at a stub end. Deduplicated per destination cluster
/// (or per node when the target is unclustered); clicking jumps there.
#[derive(Clone, Debug, PartialEq)]
pub struct ExitLabel {
    pub x: f32,
    pub y: f32,
    pub text: String,
    /// `Some` → the jump target; `None` → the destination has no cluster
    /// (label renders inert).
    pub target_cluster: Option<String>,
}

/// Exit-edge geometry for one frame.
#[derive(Clone, Debug, Default)]
pub struct ExitEdges {
    pub stubs: Vec<ExitStub>,
    pub labels: Vec<ExitLabel>,
}

/// Compute the scoped frame's exit edges: every graph edge with exactly one
/// endpoint in `members` becomes a fading stub from the in-scope node toward
/// the external one, plus one label per distinct destination (capped at
/// `max_labels`, by stub count so the busiest destinations keep their chips).
///
/// Pure + gpui-free: the canvas paint path turns stubs into faded segments,
/// the view turns labels into clickable chips. `vp` projects model→window px;
/// `fade_distance_px` comes from the Theme (`graph_panel.exit_edges`).
pub fn compute_exit_edges(
    graph: &WorkspaceGraph,
    layout: &Layout,
    members: &HashSet<String>,
    vp: &crate::graph::render::Viewport,
    fade_distance_px: f32,
    max_labels: usize,
) -> ExitEdges {
    // node id → owning cluster (for label targets).
    let mut owner: HashMap<&str, &Cluster> = HashMap::new();
    for c in &graph.clusters {
        for m in &c.member_ids {
            owner.insert(m.as_str(), c);
        }
    }

    let mut stubs = Vec::new();
    // destination key → (label text, target cluster, sum of stub ends, count)
    let mut dests: HashMap<String, (String, Option<String>, f32, f32, usize)> = HashMap::new();

    for e in &graph.edges {
        let src_in = members.contains(&e.src);
        let dst_in = members.contains(&e.dst);
        if src_in == dst_in {
            continue; // internal or fully external — not an exit edge.
        }
        let (inside, outside) = if src_in {
            (&e.src, &e.dst)
        } else {
            (&e.dst, &e.src)
        };
        let (Some(pi), Some(po)) = (layout.get(inside), layout.get(outside)) else {
            continue;
        };
        let (sx0, sy0) = vp.model_to_screen(pi);
        let (sx1, sy1) = vp.model_to_screen(po);
        let dx = sx1 - sx0;
        let dy = sy1 - sy0;
        let len = (dx * dx + dy * dy).sqrt();
        if len < f32::EPSILON {
            continue;
        }
        let reach = fade_distance_px.min(len);
        let (ex, ey) = (sx0 + dx / len * reach, sy0 + dy / len * reach);
        stubs.push(ExitStub {
            x0: sx0,
            y0: sy0,
            x1: ex,
            y1: ey,
            rel_key: e.rel_type.theme_key(),
            thickness: 1.5,
        });

        let (key, text, target) = match owner.get(outside.as_str()) {
            Some(c) => (c.id.clone(), display_name(&c.id), Some(c.id.clone())),
            None => {
                let name = graph
                    .node_by_id(outside)
                    .map(|n| n.name.clone())
                    .unwrap_or_else(|| display_name(outside));
                (format!("node:{outside}"), name, None)
            }
        };
        let entry = dests.entry(key).or_insert((text, target, 0.0, 0.0, 0));
        entry.2 += ex;
        entry.3 += ey;
        entry.4 += 1;
    }

    // One chip per destination at the mean stub end; busiest destinations
    // survive the cap.
    let mut ranked: Vec<_> = dests.into_values().collect();
    ranked.sort_by(|a, b| b.4.cmp(&a.4).then_with(|| a.0.cmp(&b.0)));
    let labels = ranked
        .into_iter()
        .take(max_labels)
        .map(|(text, target_cluster, sx, sy, n)| ExitLabel {
            x: sx / n as f32,
            y: sy / n as f32,
            text,
            target_cluster,
        })
        .collect();

    ExitEdges { stubs, labels }
}

/// Append exit stubs to a frame's draw list as solid segments with a stepped
/// alpha fade (gpui has no per-vertex alpha): `segments` pieces per stub,
/// full edge colour at the node falling to transparent at the stub end. Edge
/// colours come from the Theme per rel-type, like regular edges.
pub fn append_exit_stubs(
    out: &mut crate::graph::render::RenderOutput,
    stubs: &[ExitStub],
    theme: &crate::graph::render::Theme,
    dark: bool,
    segments: usize,
) {
    use crate::graph::render::{Color, EdgeDraw};
    let segments = segments.max(1);
    for s in stubs {
        let color = theme
            .edge_style(s.rel_key)
            .map(|st| st.color(dark))
            .unwrap_or(Color::FALLBACK);
        for i in 0..segments {
            let t0 = i as f32 / segments as f32;
            let t1 = (i + 1) as f32 / segments as f32;
            // Alpha at the segment midpoint: 1 at the node → 0 at the tip.
            let fade = 1.0 - (t0 + t1) * 0.5;
            out.edges.push(EdgeDraw {
                x0: s.x0 + (s.x1 - s.x0) * t0,
                y0: s.y0 + (s.y1 - s.y0) * t0,
                x1: s.x0 + (s.x1 - s.x0) * t1,
                y1: s.y0 + (s.y1 - s.y0) * t1,
                color: color.with_alpha(color.a * fade),
                thickness: s.thickness,
            });
        }
    }
}

// ── GraphView navigation actions ─────────────────────────────────────────

use std::time::Instant;

use gpui::Context;

use super::GraphView;

impl GraphView {
    /// Execute a [`NavAction`]: arm the matching camera tween(s) and start the
    /// main-thread driver. Exit-edge jumps chain zoom-out → zoom-in via
    /// `pending_enter` (the second tween arms when the first completes).
    pub(in crate::graph) fn apply_nav_action(&mut self, action: NavAction, cx: &mut Context<Self>) {
        let now = Instant::now();
        match action {
            NavAction::EnterCluster(id) => self.enter_cluster_by_id(&id, now),
            NavAction::LeaveScope => self.leave_scope(now),
            NavAction::JumpToCluster(id) => {
                if self.navigator.is_scoped() {
                    self.pending_enter = Some(id);
                    self.leave_scope(now);
                } else {
                    self.enter_cluster_by_id(&id, now);
                }
            }
        }
        if self.camera_transition.is_some() {
            self.spawn_camera_driver(cx);
        }
        cx.notify();
    }

    /// Scope into `cluster_id`: push it onto the navigator stack (saving the
    /// current camera as the way back) and arm the zoom-in tween to frame its
    /// bounds. No-op when the cluster is unknown, unplaced, or the canvas has
    /// not painted yet. cx-free so tests (and the tween-chain completion path)
    /// drive it directly.
    pub(in crate::graph) fn enter_cluster_by_id(&mut self, cluster_id: &str, now: Instant) {
        if self.canvas.w <= 0.0 || self.canvas.h <= 0.0 {
            return;
        }
        let Some(cluster) = self.graph.cluster_by_id(cluster_id).cloned() else {
            return;
        };
        let Some(bounds) = camera::members_bounds(&cluster.member_ids, &self.layout, 0.0) else {
            return;
        };
        let target = camera::camera_to_fit(
            bounds,
            self.canvas.w,
            self.canvas.h,
            self.navigator.config.cluster_fit_margin,
        );
        self.navigator.enter(&cluster, self.camera, target.zoom);
        self.begin_camera_tween(target, "graph_zoom_into_cluster", now);
    }

    /// Pop the innermost scope and arm the zoom-out tween back to the camera
    /// saved at entry. No-op when unscoped.
    pub(in crate::graph) fn leave_scope(&mut self, now: Instant) {
        if let Some(entry) = self.navigator.leave(&self.graph) {
            self.begin_camera_tween(entry.saved_camera, "graph_zoom_out", now);
        }
    }
}

/// Human label for a cluster id (v1 ids are directory paths): the last path
/// segment, tolerating both separators.
pub fn display_name(id: &str) -> String {
    id.trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or(id)
        .to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::{Edge, Node, NodeKind, Position, RelType};
    use crate::graph::render::Viewport;
    use std::collections::HashMap as Map;

    fn node(id: &str) -> Node {
        Node {
            id: id.to_owned(),
            kind: NodeKind::Function,
            name: id.to_owned(),
            file: format!("src/{id}.rs"),
            line: 0,
            position: Position::default(),
            style: Default::default(),
        }
    }

    fn edge(src: &str, dst: &str) -> Edge {
        Edge {
            src: src.to_owned(),
            dst: dst.to_owned(),
            rel_type: RelType::Calls,
            weight: 1.0,
        }
    }

    fn cluster(id: &str, members: &[&str], threshold: f32) -> Cluster {
        Cluster {
            id: id.to_owned(),
            member_ids: members.iter().map(|s| (*s).to_owned()).collect(),
            parent_breadcrumb: vec![],
            zoom_threshold: threshold,
        }
    }

    fn layout(items: &[(&str, f32, f32)]) -> Layout {
        let map: Map<String, Position> = items
            .iter()
            .map(|(id, x, y)| {
                (
                    (*id).to_owned(),
                    Position {
                        x: *x,
                        y: *y,
                        z: 0.0,
                    },
                )
            })
            .collect();
        Layout::from_positions(map)
    }

    fn vp() -> Viewport {
        Viewport {
            origin_x: 0.0,
            origin_y: 0.0,
            width: 800.0,
            height: 600.0,
            camera: Camera::default(),
            dark: true,
        }
    }

    /// Two clusters: alpha = {a1, a2} on the left, beta = {b1} on the right,
    /// a1 → b1 crossing between them.
    fn two_cluster_graph() -> (WorkspaceGraph, Layout) {
        let g = WorkspaceGraph {
            nodes: vec![node("a1"), node("a2"), node("b1")],
            edges: vec![edge("a1", "a2"), edge("a1", "b1")],
            clusters: vec![
                cluster("ws/alpha", &["a1", "a2"], 1.0),
                cluster("ws/beta", &["b1"], 1.0),
            ],
        };
        let l = layout(&[("a1", -100.0, 0.0), ("a2", -80.0, 30.0), ("b1", 150.0, 0.0)]);
        (g, l)
    }

    #[test]
    fn enters_on_threshold_crossing_under_cursor() {
        let (g, l) = two_cluster_graph();
        let nav = Navigator::default();
        // Cursor over alpha's bounds, zoom crossing 1.0 from below.
        let action = nav.action_for_zoom(0.9, 1.1, (-90.0, 10.0), &g, &l);
        assert_eq!(action, Some(NavAction::EnterCluster("ws/alpha".into())));
    }

    #[test]
    fn no_enter_when_cursor_outside_cluster() {
        let (g, l) = two_cluster_graph();
        let nav = Navigator::default();
        let action = nav.action_for_zoom(0.9, 1.1, (0.0, -200.0), &g, &l);
        assert_eq!(action, None);
    }

    #[test]
    fn no_enter_without_crossing() {
        let (g, l) = two_cluster_graph();
        let nav = Navigator::default();
        // Already above threshold — zooming further in does not enter…
        assert_eq!(nav.action_for_zoom(1.2, 1.4, (-90.0, 10.0), &g, &l), None);
        // …and zooming out never enters.
        assert_eq!(nav.action_for_zoom(1.4, 0.9, (-90.0, 10.0), &g, &l), None);
    }

    #[test]
    fn smallest_cluster_wins_on_overlap() {
        let g = WorkspaceGraph {
            nodes: vec![node("x1"), node("x2"), node("y1")],
            edges: vec![],
            clusters: vec![
                // big spans (-100..100); small spans (-10..10) inside it.
                cluster("ws/big", &["x1", "x2"], 1.0),
                cluster("ws/small", &["y1", "x1"], 1.0),
            ],
        };
        let l = layout(&[("x1", -10.0, 0.0), ("x2", 100.0, 80.0), ("y1", 10.0, 5.0)]);
        let nav = Navigator::default();
        let action = nav.action_for_zoom(0.9, 1.1, (0.0, 2.0), &g, &l);
        assert_eq!(action, Some(NavAction::EnterCluster("ws/small".into())));
    }

    #[test]
    fn scoped_leaves_below_hysteresis_and_not_above() {
        let (g, l) = two_cluster_graph();
        let mut nav = Navigator::default();
        let c = g.cluster_by_id("ws/alpha").unwrap();
        nav.enter(c, Camera::default(), 3.0);
        // leave_below = min(1.0, 3.0) × 0.8 = 0.8.
        assert_eq!(nav.action_for_zoom(1.0, 0.9, (0.0, 0.0), &g, &l), None);
        assert_eq!(
            nav.action_for_zoom(0.9, 0.7, (0.0, 0.0), &g, &l),
            Some(NavAction::LeaveScope)
        );
        // While scoped, threshold crossings don't enter (nested = C-cluster).
        assert_eq!(nav.action_for_zoom(0.9, 1.5, (150.0, 0.0), &g, &l), None);
    }

    #[test]
    fn big_cluster_fit_zoom_guards_leave_point() {
        let (g, _) = two_cluster_graph();
        let mut nav = Navigator::default();
        let c = g.cluster_by_id("ws/alpha").unwrap();
        // Fit zoom (0.5) lands BELOW the threshold (1.0): leave_below must
        // derive from the fit zoom, or the first scroll-out would exit.
        nav.enter(c, Camera::default(), 0.5);
        assert!((nav.scope().unwrap().leave_below - 0.4).abs() < 1e-6);
    }

    #[test]
    fn enter_leave_round_trip_restores_camera_and_members() {
        let (g, _) = two_cluster_graph();
        let mut nav = Navigator::default();
        let saved = Camera {
            pan_x: 42.0,
            pan_y: -7.0,
            zoom: 0.95,
        };
        nav.enter(g.cluster_by_id("ws/alpha").unwrap(), saved, 3.0);
        assert!(nav.is_scoped());
        let m = nav.members().unwrap();
        assert!(m.contains("a1") && m.contains("a2") && !m.contains("b1"));

        let popped = nav.leave(&g).unwrap();
        assert_eq!(popped.saved_camera, saved);
        assert!(!nav.is_scoped() && nav.members().is_none());
        assert!(nav.leave(&g).is_none(), "leave on empty stack is a no-op");
    }

    #[test]
    fn reset_clears_scope() {
        let (g, _) = two_cluster_graph();
        let mut nav = Navigator::default();
        nav.enter(g.cluster_by_id("ws/beta").unwrap(), Camera::default(), 2.0);
        nav.reset();
        assert!(!nav.is_scoped() && nav.members().is_none());
    }

    // ── exit edges ───────────────────────────────────────────────────────

    #[test]
    fn exit_edges_detect_one_in_one_out_only() {
        let (g, l) = two_cluster_graph();
        let members: HashSet<String> = ["a1", "a2"].iter().map(|s| (*s).to_owned()).collect();
        let xe = compute_exit_edges(&g, &l, &members, &vp(), 40.0, 12);
        // a1→a2 internal (skipped); a1→b1 exits.
        assert_eq!(xe.stubs.len(), 1);
        assert_eq!(xe.labels.len(), 1);
        assert_eq!(xe.labels[0].text, "beta");
        assert_eq!(xe.labels[0].target_cluster.as_deref(), Some("ws/beta"));
    }

    #[test]
    fn exit_stub_is_fade_distance_long_toward_target() {
        let (g, l) = two_cluster_graph();
        let members: HashSet<String> = ["a1", "a2"].iter().map(|s| (*s).to_owned()).collect();
        let v = vp();
        let xe = compute_exit_edges(&g, &l, &members, &v, 40.0, 12);
        let s = &xe.stubs[0];
        let len = ((s.x1 - s.x0).powi(2) + (s.y1 - s.y0).powi(2)).sqrt();
        assert!((len - 40.0).abs() < 1e-3, "stub length = fade distance");
        // Points right (toward b1 at +x).
        assert!(s.x1 > s.x0);
        assert!((s.y1 - s.y0).abs() < 1e-3);
        // Starts at a1's screen position.
        let (ax, ay) = v.model_to_screen(l.get("a1").unwrap());
        assert!((s.x0 - ax).abs() < 1e-3 && (s.y0 - ay).abs() < 1e-3);
    }

    #[test]
    fn exit_labels_dedupe_per_destination_cluster() {
        let g = WorkspaceGraph {
            nodes: vec![node("a1"), node("a2"), node("b1"), node("b2")],
            edges: vec![edge("a1", "b1"), edge("a2", "b2"), edge("b1", "a2")],
            clusters: vec![
                cluster("ws/alpha", &["a1", "a2"], 1.0),
                cluster("ws/beta", &["b1", "b2"], 1.0),
            ],
        };
        let l = layout(&[
            ("a1", -100.0, -20.0),
            ("a2", -100.0, 20.0),
            ("b1", 150.0, -20.0),
            ("b2", 150.0, 20.0),
        ]);
        let members: HashSet<String> = ["a1", "a2"].iter().map(|s| (*s).to_owned()).collect();
        let xe = compute_exit_edges(&g, &l, &members, &vp(), 40.0, 12);
        // Three crossing edges (direction doesn't matter) → 3 stubs, 1 label.
        assert_eq!(xe.stubs.len(), 3);
        assert_eq!(xe.labels.len(), 1);
        assert_eq!(xe.labels[0].text, "beta");
    }

    #[test]
    fn unclustered_destination_gets_inert_node_label() {
        let g = WorkspaceGraph {
            nodes: vec![node("a1"), node("free")],
            edges: vec![edge("a1", "free")],
            clusters: vec![cluster("ws/alpha", &["a1"], 1.0)],
        };
        let l = layout(&[("a1", 0.0, 0.0), ("free", 100.0, 0.0)]);
        let members: HashSet<String> = ["a1"].iter().map(|s| (*s).to_owned()).collect();
        let xe = compute_exit_edges(&g, &l, &members, &vp(), 40.0, 12);
        assert_eq!(xe.labels.len(), 1);
        assert_eq!(xe.labels[0].text, "free");
        assert_eq!(xe.labels[0].target_cluster, None);
    }

    #[test]
    fn label_cap_keeps_busiest_destinations() {
        // 3 destinations with 3, 2, 1 stubs; cap at 2 → the singleton drops.
        let mut nodes = vec![node("in")];
        let mut edges = Vec::new();
        let mut clusters = vec![cluster("ws/in", &["in"], 1.0)];
        let mut items = vec![("in", 0.0_f32, 0.0_f32)];
        for (ci, count) in [("c3", 3usize), ("c2", 2), ("c1", 1)] {
            let mut ids = Vec::new();
            for i in 0..count {
                let id = format!("{ci}-n{i}");
                nodes.push(node(&id));
                edges.push(edge("in", &id));
                items.push((
                    Box::leak(id.clone().into_boxed_str()),
                    100.0,
                    i as f32 * 10.0,
                ));
                ids.push(id);
            }
            let refs: Vec<&str> = ids.iter().map(String::as_str).collect();
            clusters.push(cluster(&format!("ws/{ci}"), &refs, 1.0));
        }
        let g = WorkspaceGraph {
            nodes,
            edges,
            clusters,
        };
        let l = layout(&items);
        let members: HashSet<String> = ["in"].iter().map(|s| (*s).to_owned()).collect();
        let xe = compute_exit_edges(&g, &l, &members, &vp(), 40.0, 2);
        assert_eq!(xe.stubs.len(), 6, "stubs are never capped");
        let texts: Vec<&str> = xe.labels.iter().map(|l| l.text.as_str()).collect();
        assert_eq!(texts.len(), 2);
        assert!(texts.contains(&"c3") && texts.contains(&"c2"));
    }

    #[test]
    fn exit_edges_scale_to_synthetic_graph_inside_frame_budget() {
        // Perf sanity (§2.5 frame budget 16 ms): 1500 nodes / 3000 edges with
        // a 50-member scope computes well inside one frame even in debug.
        let mut nodes = Vec::new();
        let mut edges = Vec::new();
        let mut items = Vec::new();
        let mut member_ids = Vec::new();
        for i in 0..1500 {
            let id = format!("n{i:04}");
            nodes.push(node(&id));
            items.push((
                Box::leak(id.clone().into_boxed_str()) as &str,
                (i % 50) as f32 * 20.0,
                (i / 50) as f32 * 20.0,
            ));
            if i < 50 {
                member_ids.push(id.clone());
            }
            if i > 0 {
                edges.push(edge(&format!("n{:04}", i - 1), &id));
                edges.push(edge(&format!("n{:04}", i / 2), &id));
            }
        }
        let refs: Vec<&str> = member_ids.iter().map(String::as_str).collect();
        let g = WorkspaceGraph {
            nodes,
            edges,
            clusters: vec![cluster("ws/scope", &refs, 1.0)],
        };
        let l = layout(&items);
        let members: HashSet<String> = member_ids.into_iter().collect();

        let start = std::time::Instant::now();
        let xe = compute_exit_edges(&g, &l, &members, &vp(), 40.0, 12);
        let took = start.elapsed();
        assert!(!xe.stubs.is_empty());
        assert!(
            took.as_millis() < 16,
            "exit-edge pass inside the 16 ms frame budget (got {took:?})"
        );
    }

    #[test]
    fn append_exit_stubs_fades_to_transparent() {
        use crate::graph::render::{RenderOutput, Theme};
        let theme = Theme::load_v1().unwrap();
        let stub = ExitStub {
            x0: 0.0,
            y0: 0.0,
            x1: 40.0,
            y1: 0.0,
            rel_key: "calls",
            thickness: 1.5,
        };
        let mut out = RenderOutput::default();
        append_exit_stubs(&mut out, &[stub], &theme, true, 6);
        assert_eq!(out.edges.len(), 6, "one EdgeDraw per fade segment");
        // Alphas strictly decrease toward the tip; the last is near zero.
        let alphas: Vec<f32> = out.edges.iter().map(|e| e.color.a).collect();
        for w in alphas.windows(2) {
            assert!(w[1] < w[0], "fade decreases: {alphas:?}");
        }
        assert!(*alphas.last().unwrap() < 0.15);
        // Segments tile the stub end to end.
        assert_eq!(out.edges.first().unwrap().x0, 0.0);
        assert!((out.edges.last().unwrap().x1 - 40.0).abs() < 1e-3);
    }

    #[test]
    fn display_name_handles_both_separators() {
        assert_eq!(display_name("C:/ws/src"), "src");
        assert_eq!(display_name("C:\\ws\\graph"), "graph");
        assert_eq!(display_name("plain"), "plain");
        assert_eq!(display_name("trail/"), "trail");
    }
}
