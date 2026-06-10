//! Auto-clustering + expand-in-place (Slice C-cluster, Build Order §4).
//!
//! WHICH nodes group. For huge graphs (OQ6) the flat view folds the coldest
//! corners of the map into single **cluster spheres** — threshold-driven by
//! [`strategy`] (deepest + lowest-centrality first), assigned once per load
//! by [`precompute`]. Clusters **unfold as the camera zooms past their
//! `zoom_threshold`** (the space-map "galaxy → symbols" feel) and the user
//! can override either way via right-click → Expand / Collapse Cluster, with
//! the Theme's `cluster_expand_in_place` animation (members fly out of the
//! sphere in place — [`expand`]).
//!
//! The integration seam is [`ClusterView::apply`]: a pure **display-graph
//! transform**. The real graph + layout stay untouched (physics simulates
//! the full graph); rendering swaps in a derived graph where folded members
//! are replaced by one synthetic `Module` node per cluster
//! (id `cluster::<cluster-id>`) and boundary-crossing edges re-route to it.
//! The standard renderer, hit-testing, and drag plumbing all work unchanged
//! on the derived graph.
//!
//! Scoped space-map views (C-navigation) bypass clustering entirely — a
//! scope already filters to one cluster's members, so folding the rest buys
//! nothing inside it.

pub mod config;
pub mod expand;
pub mod precompute;
pub mod strategy;

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use crate::graph::layout::CubicBezier;
use crate::graph::model::{Edge, Layout, Node, NodeKind, Position, WorkspaceGraph};

pub use config::ClusterConfig;
use expand::ExpandAnim;
pub use expand::Override;
use precompute::ClusterIndex;

/// Synthetic cluster-sphere node ids are `cluster::<cluster-id>` so the
/// existing hit-test path distinguishes them from real nodes.
pub const CLUSTER_NODE_PREFIX: &str = "cluster::";

pub fn cluster_node_id(cluster_id: &str) -> String {
    format!("{CLUSTER_NODE_PREFIX}{cluster_id}")
}

/// The cluster id behind a synthetic node id, if it is one.
pub fn cluster_id_from_node(node_id: &str) -> Option<&str> {
    node_id.strip_prefix(CLUSTER_NODE_PREFIX)
}

/// Per-cluster fold state: current expansion progress (0 = folded sphere,
/// 1 = fully expanded) plus the in-flight tween, if any.
#[derive(Clone, Copy, Debug)]
struct FoldState {
    progress: f32,
    anim: Option<(ExpandAnim, Instant)>,
}

/// Owns clustering state for the graph view: the one-time index, the
/// auto-fold selection, user overrides, and the per-cluster expand tweens.
#[derive(Default)]
pub struct ClusterView {
    pub config: ClusterConfig,
    index: ClusterIndex,
    auto_folds: HashSet<String>,
    thresholds: HashMap<String, f32>,
    overrides: HashMap<String, Override>,
    /// Clusters currently folded or mid-animation. Absent = fully expanded.
    states: HashMap<String, FoldState>,
}

impl ClusterView {
    /// Whether auto-clustering selected anything for this graph.
    pub fn is_active(&self) -> bool {
        !self.auto_folds.is_empty()
    }

    /// Number of clusters currently rendered folded (progress 0).
    pub fn folded_count(&self) -> usize {
        self.states.values().filter(|s| s.progress <= 0.0).count()
    }

    /// True while any expand/collapse tween runs (drives the anim loop).
    pub fn is_animating(&self) -> bool {
        self.states.values().any(|s| s.anim.is_some())
    }

    /// Can the user manually fold/unfold this cluster? (Only auto-selected
    /// clusters — folding an always-flat cluster isn't meaningful.)
    pub fn is_expandable(&self, cluster_id: &str) -> bool {
        self.auto_folds.contains(cluster_id)
    }

    /// Is the cluster currently rendered as a folded sphere?
    pub fn is_folded(&self, cluster_id: &str) -> bool {
        self.states
            .get(cluster_id)
            .is_some_and(|s| s.progress <= 0.0)
    }

    /// The cluster owning a (real) node, if any.
    pub fn cluster_of(&self, node_id: &str) -> Option<&str> {
        self.index.assignment.get(node_id).map(String::as_str)
    }

    /// Rebuild for a freshly loaded graph: one-time assignment, auto-fold
    /// selection, thresholds. The initial fold set snaps into place without
    /// animation (the galaxy view IS the first paint); overrides reset.
    pub fn rebuild(&mut self, graph: &WorkspaceGraph, zoom: f32) {
        self.index = ClusterIndex::build(graph);
        self.thresholds = graph
            .clusters
            .iter()
            .map(|c| (c.id.clone(), c.zoom_threshold))
            .collect();
        self.auto_folds = strategy::select_folds(graph.nodes.len(), &self.index, &self.config);
        self.overrides.clear();
        self.states.clear();
        for id in expand::desired_folds(&self.auto_folds, &self.thresholds, zoom, &self.overrides) {
            self.states.insert(
                id,
                FoldState {
                    progress: 0.0,
                    anim: None,
                },
            );
        }
    }

    /// Snap the fold set to `zoom` without animation — used at the first
    /// paint's camera fit (the galaxy view IS the initial frame; nothing to
    /// animate from).
    pub fn snap_to(&mut self, zoom: f32) {
        let desired =
            expand::desired_folds(&self.auto_folds, &self.thresholds, zoom, &self.overrides);
        self.states.clear();
        for id in desired {
            self.states.insert(
                id,
                FoldState {
                    progress: 0.0,
                    anim: None,
                },
            );
        }
    }

    /// Re-resolve the desired fold set after a zoom change or an override,
    /// arming expand/collapse tweens for clusters whose state flips. `anim`
    /// is `(duration_ms, easing)` from the Theme (`cluster_expand_in_place`).
    /// Returns true when at least one tween was armed.
    pub fn sync(&mut self, zoom: f32, now: Instant, anim: (f32, CubicBezier)) -> bool {
        expand::prune_overrides(&mut self.overrides, &self.thresholds, zoom);
        let desired =
            expand::desired_folds(&self.auto_folds, &self.thresholds, zoom, &self.overrides);
        let mut armed = false;

        // Clusters that should be folded: collapse them from wherever they are.
        for id in &desired {
            let state = self.states.entry(id.clone()).or_insert(FoldState {
                progress: 1.0,
                anim: None,
            });
            let collapsing_already = state
                .anim
                .map(|(a, _)| !a.is_expanding())
                .unwrap_or(state.progress <= 0.0);
            if collapsing_already {
                continue;
            }
            state.anim = Some((
                ExpandAnim {
                    from: state.progress,
                    to: 0.0,
                    duration_ms: anim.0,
                    easing: anim.1,
                },
                now,
            ));
            armed = true;
        }

        // Tracked clusters that should be expanded: expand, then forget them
        // once the tween lands (apply() treats untracked as fully expanded).
        for (id, state) in self.states.iter_mut() {
            if desired.contains(id) {
                continue;
            }
            let expanding_already = state
                .anim
                .map(|(a, _)| a.is_expanding())
                .unwrap_or(state.progress >= 1.0);
            if expanding_already {
                continue;
            }
            state.anim = Some((
                ExpandAnim {
                    from: state.progress,
                    to: 1.0,
                    duration_ms: anim.0,
                    easing: anim.1,
                },
                now,
            ));
            armed = true;
        }
        armed
    }

    /// Set a user override (right-click menu) — pair with [`sync`](Self::sync)
    /// to arm the tween.
    pub fn set_override(&mut self, cluster_id: &str, ov: Override) {
        self.overrides.insert(cluster_id.to_owned(), ov);
    }

    /// Advance all tweens to wall-clock `now`; finished expansions drop out
    /// of tracking. Returns true while anything is still animating.
    pub fn advance(&mut self, now: Instant) -> bool {
        for state in self.states.values_mut() {
            if let Some((anim, start)) = state.anim {
                let elapsed = now.saturating_duration_since(start).as_secs_f32() * 1000.0;
                state.progress = anim.progress(elapsed);
                if anim.is_done(elapsed) {
                    state.progress = anim.to;
                    state.anim = None;
                }
            }
        }
        self.states
            .retain(|_, s| s.anim.is_some() || s.progress < 1.0);
        self.is_animating()
    }

    /// The display-graph transform. `None` when nothing is folded or
    /// animating (render the real graph — zero cost). Otherwise a derived
    /// `(graph, layout)`:
    ///
    /// * fully-folded clusters (progress 0): members hidden, one synthetic
    ///   `Module` node (`cluster::<id>`) at the member centroid;
    /// * animating clusters: members visible at positions lerped
    ///   centroid → layout by the expansion progress;
    /// * edges into hidden members re-route to the cluster node, deduped;
    ///   edges fully inside one hidden cluster disappear.
    pub fn apply(
        &self,
        graph: &WorkspaceGraph,
        layout: &Layout,
    ) -> Option<(WorkspaceGraph, Layout)> {
        if self.states.is_empty() {
            return None;
        }

        // node id → synthetic id (for hidden members) / position override.
        let mut reroute: HashMap<&str, String> = HashMap::new();
        let mut positions: HashMap<String, Position> = HashMap::new();
        for (id, p) in layout.iter() {
            positions.insert(id.clone(), *p);
        }
        let mut synthetic_nodes: Vec<Node> = Vec::new();

        for (cid, state) in &self.states {
            let Some(centroid) = self.index.centroid(cid, layout) else {
                continue;
            };
            let members = match self.index.members.get(cid) {
                Some(m) => m,
                None => continue,
            };
            if state.progress <= 0.0 {
                // Folded: hide members, draw the cluster sphere.
                let synth = cluster_node_id(cid);
                for m in members {
                    reroute.insert(m.as_str(), synth.clone());
                    positions.remove(m);
                }
                synthetic_nodes.push(Node {
                    id: synth.clone(),
                    kind: NodeKind::Module,
                    name: super::navigation::display_name(cid),
                    file: cid.clone(),
                    line: 0,
                    position: Position::default(),
                    style: Default::default(),
                });
                positions.insert(synth, centroid);
            } else if state.progress < 1.0 {
                // Mid-animation: members fly centroid ↔ layout position.
                for m in members {
                    if let Some(target) = layout.get(m) {
                        positions.insert(
                            m.clone(),
                            expand::member_position(centroid, target, state.progress),
                        );
                    }
                }
            }
        }

        // Derived node list: visible real nodes + cluster spheres.
        let mut nodes: Vec<Node> = graph
            .nodes
            .iter()
            .filter(|n| !reroute.contains_key(n.id.as_str()))
            .cloned()
            .collect();
        nodes.extend(synthetic_nodes);

        // Re-routed, deduped edges.
        let mut seen: HashSet<(String, String, &'static str)> = HashSet::new();
        let mut edges: Vec<Edge> = Vec::new();
        for e in &graph.edges {
            let src = reroute.get(e.src.as_str()).unwrap_or(&e.src);
            let dst = reroute.get(e.dst.as_str()).unwrap_or(&e.dst);
            if src == dst {
                continue; // collapsed into one sphere (or a self-loop)
            }
            if !seen.insert((src.clone(), dst.clone(), e.rel_type.theme_key())) {
                continue;
            }
            edges.push(Edge {
                src: src.clone(),
                dst: dst.clone(),
                rel_type: e.rel_type,
                weight: e.weight,
            });
        }

        Some((
            WorkspaceGraph {
                nodes,
                edges,
                clusters: graph.clusters.clone(),
            },
            Layout::from_positions(positions),
        ))
    }

    /// Boundary outlines (Theme `cluster_boundary`) around clusters the user
    /// expanded **in place** (override = Expanded): the visual containment cue
    /// for "these children were revealed alongside". Model-space rects with
    /// `pad` (config) around member bounds; the view projects to window px.
    pub fn expanded_boundaries(&self, layout: &Layout) -> Vec<(String, (f32, f32, f32, f32))> {
        let mut out = Vec::new();
        for (id, ov) in &self.overrides {
            if *ov != Override::Expanded {
                continue;
            }
            // Only once fully expanded (no half-drawn boxes mid-flight).
            let tracked = self.states.get(id);
            if tracked.is_some_and(|s| s.progress < 1.0) {
                continue;
            }
            let Some(members) = self.index.members.get(id) else {
                continue;
            };
            if let Some(bb) = super::navigation::camera::members_bounds(
                members,
                layout,
                self.config.boundary_pad_px,
            ) {
                out.push((id.clone(), bb));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::{Cluster, RelType};
    use std::time::Duration;

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

    /// 12 nodes: cold = {c0..c5} (foldable), hot = {h0..h3} + 2 loose nodes.
    /// Config tuned so auto-clustering arms and folds exactly `cold`.
    fn test_view() -> (ClusterView, WorkspaceGraph, Layout) {
        let mut g = WorkspaceGraph::default();
        let mut pos = HashMap::new();
        for i in 0..6 {
            let id = format!("c{i}");
            g.nodes.push(node(&id));
            pos.insert(
                id,
                Position {
                    x: 100.0 + i as f32 * 10.0,
                    y: 50.0,
                    z: 0.0,
                },
            );
        }
        for i in 0..4 {
            let id = format!("h{i}");
            g.nodes.push(node(&id));
            pos.insert(
                id,
                Position {
                    x: -100.0,
                    y: i as f32 * 10.0,
                    z: 0.0,
                },
            );
        }
        g.nodes.push(node("loose1"));
        g.nodes.push(node("loose2"));
        pos.insert(
            "loose1".to_owned(),
            Position {
                x: 0.0,
                y: 0.0,
                z: 0.0,
            },
        );
        pos.insert(
            "loose2".to_owned(),
            Position {
                x: 0.0,
                y: 20.0,
                z: 0.0,
            },
        );
        // Heat up `hot` so `cold` ranks first; cross edges for re-routing.
        for i in 0..3 {
            g.edges.push(edge(&format!("h{i}"), &format!("h{}", i + 1)));
        }
        g.edges.push(edge("c0", "c1")); // internal to cold → disappears folded
        g.edges.push(edge("c2", "h0")); // cold → hot: re-routes
        g.edges.push(edge("c3", "h0")); // same target: dedupes with previous
        g.edges.push(edge("loose1", "c5")); // loose → cold: re-routes

        g.clusters.push(cluster(
            "ws/cold",
            &["c0", "c1", "c2", "c3", "c4", "c5"],
            2.0,
        ));
        g.clusters
            .push(cluster("ws/hot", &["h0", "h1", "h2", "h3"], 2.0));

        let mut cv = ClusterView {
            config: ClusterConfig {
                auto_threshold_nodes: 10,
                target_visible_nodes: 8,
                min_fold_size: 3,
                boundary_pad_px: 18.0,
            },
            ..Default::default()
        };
        cv.rebuild(&g, 1.0);
        (cv, g, Layout::from_positions(pos))
    }

    fn anim() -> (f32, CubicBezier) {
        (300.0, expand::EXPAND_FALLBACK_EASING)
    }

    #[test]
    fn rebuild_folds_cold_cluster_immediately() {
        let (cv, _, _) = test_view();
        assert!(cv.is_active());
        // 12 nodes → fold ws/cold (deeper tie broken by lower degree) → 7 ≤ 8.
        assert!(cv.is_folded("ws/cold"));
        assert!(!cv.is_folded("ws/hot"));
        assert_eq!(cv.folded_count(), 1);
        assert!(cv.is_expandable("ws/cold"));
        assert!(!cv.is_animating(), "initial fold snaps, no tween");
    }

    #[test]
    fn apply_builds_display_graph_with_cluster_sphere() {
        let (cv, g, l) = test_view();
        let (dg, dl) = cv.apply(&g, &l).expect("folded → derived graph");
        // 12 real − 6 hidden + 1 sphere = 7 nodes.
        assert_eq!(dg.nodes.len(), 7);
        let synth = dg
            .nodes
            .iter()
            .find(|n| n.id == "cluster::ws/cold")
            .expect("synthetic node");
        assert_eq!(synth.kind, NodeKind::Module);
        assert_eq!(synth.name, "cold");
        // Sphere sits at the members' centroid (x = 125, y = 50).
        let p = dl.get("cluster::ws/cold").unwrap();
        assert!((p.x - 125.0).abs() < 1e-3 && (p.y - 50.0).abs() < 1e-3);
        // Hidden member has no position in the display layout.
        assert!(dl.get("c0").is_none());

        // Edges: h-chain ×3 kept; c0→c1 vanished; c2/c3→h0 deduped to one
        // cluster::ws/cold→h0; loose1→c5 re-routed.
        assert_eq!(dg.edges.len(), 5);
        assert!(dg
            .edges
            .iter()
            .any(|e| e.src == "cluster::ws/cold" && e.dst == "h0"));
        assert!(dg
            .edges
            .iter()
            .any(|e| e.src == "loose1" && e.dst == "cluster::ws/cold"));
    }

    #[test]
    fn zoom_past_threshold_unfolds_with_animation() {
        let (mut cv, g, l) = test_view();
        let t0 = Instant::now();
        // Zoom past the 2.0 threshold → expand tween arms.
        assert!(cv.sync(2.5, t0, anim()));
        assert!(cv.is_animating());

        // Mid-flight: members visible at lerped positions.
        cv.advance(t0 + Duration::from_millis(150));
        let (dg, dl) = cv.apply(&g, &l).expect("still tracked mid-anim");
        assert!(dg.nodes.iter().any(|n| n.id == "c0"), "members visible");
        assert!(!dg.nodes.iter().any(|n| n.id == "cluster::ws/cold"));
        let mid = dl.get("c0").unwrap();
        let target = l.get("c0").unwrap();
        let centroid_x = 125.0;
        assert!(
            (mid.x - target.x).abs() > 1e-3 && (mid.x - centroid_x).abs() > 1e-3,
            "strictly between centroid and target"
        );

        // Completion: tracking drops, apply() returns None (flat path).
        cv.advance(t0 + Duration::from_millis(400));
        assert!(!cv.is_animating());
        assert!(cv.apply(&g, &l).is_none());

        // Zooming back out re-folds (collapse tween → folded sphere).
        let t1 = t0 + Duration::from_millis(500);
        assert!(cv.sync(1.0, t1, anim()));
        cv.advance(t1 + Duration::from_millis(400));
        assert!(cv.is_folded("ws/cold"));
    }

    #[test]
    fn expand_override_unfolds_below_threshold() {
        let (mut cv, g, l) = test_view();
        let t0 = Instant::now();
        cv.set_override("ws/cold", Override::Expanded);
        assert!(cv.sync(1.0, t0, anim()), "tween armed at low zoom");
        cv.advance(t0 + Duration::from_millis(400));
        assert!(!cv.is_folded("ws/cold"));
        assert!(cv.apply(&g, &l).is_none(), "fully expanded → flat path");

        // Boundary outline appears for the in-place expansion.
        let bounds = cv.expanded_boundaries(&l);
        assert_eq!(bounds.len(), 1);
        assert_eq!(bounds[0].0, "ws/cold");
        let bb = bounds[0].1;
        // Members span x 100..150, y 50; pad 18.
        assert!((bb.0 - 82.0).abs() < 1e-3 && (bb.2 - 168.0).abs() < 1e-3);
    }

    #[test]
    fn collapse_override_folds_above_threshold() {
        let (mut cv, _, _) = test_view();
        let t0 = Instant::now();
        // Unfold via zoom first.
        cv.sync(2.5, t0, anim());
        cv.advance(t0 + Duration::from_millis(400));
        // User collapses it back while still zoomed in.
        let t1 = t0 + Duration::from_millis(500);
        cv.set_override("ws/cold", Override::Collapsed);
        assert!(cv.sync(2.5, t1, anim()));
        cv.advance(t1 + Duration::from_millis(400));
        assert!(cv.is_folded("ws/cold"));
        // No boundary for a collapsed cluster.
        assert!(cv.expanded_boundaries(&Layout::default()).is_empty());
    }

    #[test]
    fn mid_flight_reversal_is_smooth_not_snapping() {
        let (mut cv, _, _) = test_view();
        let t0 = Instant::now();
        cv.sync(2.5, t0, anim()); // expanding
        cv.advance(t0 + Duration::from_millis(150));
        let mid = cv.states["ws/cold"].progress;
        assert!(mid > 0.0 && mid < 1.0);

        // Reverse: zoom back below threshold mid-flight.
        let t1 = t0 + Duration::from_millis(150);
        assert!(cv.sync(1.0, t1, anim()));
        let s = cv.states["ws/cold"];
        let (a, _) = s.anim.unwrap();
        assert!((a.from - mid).abs() < 1e-4, "collapse starts from {mid}");
        assert_eq!(a.to, 0.0);
    }

    #[test]
    fn small_graph_apply_is_none() {
        let mut g = WorkspaceGraph::default();
        for i in 0..5 {
            g.nodes.push(node(&format!("n{i}")));
        }
        g.clusters
            .push(cluster("ws/all", &["n0", "n1", "n2", "n3", "n4"], 1.0));
        let mut cv = ClusterView::default();
        cv.rebuild(&g, 1.0);
        assert!(!cv.is_active());
        assert!(cv.apply(&g, &Layout::default()).is_none());
    }

    #[test]
    fn cluster_node_id_round_trips() {
        let id = cluster_node_id("ws/x");
        assert_eq!(id, "cluster::ws/x");
        assert_eq!(cluster_id_from_node(&id), Some("ws/x"));
        assert_eq!(cluster_id_from_node("plain"), None);
    }

    #[test]
    fn display_transform_scales_inside_frame_budget() {
        // Perf sanity (§2.5): 1500 nodes / 3000 edges, 20 folded clusters.
        let mut g = WorkspaceGraph::default();
        let mut pos = HashMap::new();
        for ci in 0..30 {
            let mut members = Vec::new();
            for i in 0..50 {
                let id = format!("c{ci}-n{i}");
                g.nodes.push(node(&id));
                pos.insert(
                    id.clone(),
                    Position {
                        x: ci as f32 * 100.0,
                        y: i as f32 * 10.0,
                        z: 0.0,
                    },
                );
                members.push(id);
            }
            g.clusters.push(Cluster {
                id: format!("ws/c{ci}"),
                member_ids: members,
                parent_breadcrumb: vec![],
                zoom_threshold: 1.0,
            });
        }
        for i in 1..1500usize {
            g.edges.push(edge(
                &format!("c{}-n{}", (i - 1) / 50, (i - 1) % 50),
                &format!("c{}-n{}", i / 50, i % 50),
            ));
            g.edges.push(edge(
                &format!("c{}-n{}", (i / 2) / 50, (i / 2) % 50),
                &format!("c{}-n{}", i / 50, i % 50),
            ));
        }
        let layout = Layout::from_positions(pos);
        let mut cv = ClusterView::default(); // defaults: arms at >300 nodes
        cv.rebuild(&g, 0.5);
        assert!(cv.is_active());
        assert!(cv.folded_count() > 0);

        let start = Instant::now();
        let derived = cv.apply(&g, &layout).expect("folded");
        let took = start.elapsed();
        assert!(derived.0.nodes.len() < g.nodes.len());
        assert!(
            took.as_millis() < 16,
            "display transform inside the 16 ms frame budget (got {took:?})"
        );
    }
}
