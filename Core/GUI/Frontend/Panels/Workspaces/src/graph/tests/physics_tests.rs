//! Cross-module physics suite (Build Order §4 file tree → `graph/tests/`):
//! the **integration** test (a small graph settles into a recognisable layered
//! structure) and the **perf** test (a 1500-node step stays inside the §2.5
//! settle budget). Per-module force / Barnes-Hut / damping unit tests live next
//! to their code (`physics/{forces,barnes_hut,damping,mod}.rs`).

use std::time::Instant;

use crate::graph::layout::{ForceDirected, LayoutConfig};
use crate::graph::model::{Edge, Node, NodeKind, Position, RelType, WorkspaceGraph};
use crate::graph::physics::PhysicsConfig;

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

fn calls(src: &str, dst: &str) -> Edge {
    Edge {
        src: src.to_owned(),
        dst: dst.to_owned(),
        rel_type: RelType::Calls,
        weight: 1.0,
    }
}

/// A 10-node / 8-edge dependency tree plus one isolated node:
///
/// ```text
/// depth 0:            r                z (isolated)
/// depth 1:        a       b
/// depth 2:      c   d   e   f
/// depth 3:    g       h
/// ```
fn layered_graph() -> WorkspaceGraph {
    WorkspaceGraph {
        nodes: ["r", "a", "b", "c", "d", "e", "f", "g", "h", "z"]
            .iter()
            .map(|id| node(id))
            .collect(),
        edges: vec![
            calls("r", "a"),
            calls("r", "b"),
            calls("a", "c"),
            calls("a", "d"),
            calls("b", "e"),
            calls("b", "f"),
            calls("c", "g"),
            calls("d", "h"),
        ],
        clusters: vec![],
    }
}

#[test]
fn small_graph_settles_into_a_centred_radial_structure() {
    // Center-anchor layout (viz-fix): the depth bands become concentric rings
    // around the origin. Roots collapse to the centre; dependencies fan out;
    // and — the key fix — the isolated node `z` is tethered near the centre
    // instead of being flung off-screen by repulsion.
    let g = layered_graph();
    let fd = ForceDirected::default();
    let mut engine = fd.build_engine(&g, PhysicsConfig::default());

    // Run for up to ~5 s of simulated frames (300 @ 60 fps); the annealed sim
    // should freeze well inside that (~2 s).
    let mut steps = 0;
    for _ in 0..300 {
        steps += 1;
        if engine.step().settled {
            break;
        }
    }
    assert!(
        engine.settled(),
        "graph reached equilibrium in {steps} steps"
    );
    assert!(steps < 300, "settled before the 5 s cap (took {steps})");

    let layout = engine.snapshot().to_layout();
    let radius = |id: &str| {
        let p = layout.get(id).unwrap();
        (p.x * p.x + p.y * p.y).sqrt()
    };
    let mean_r = |ids: &[&str]| ids.iter().map(|i| radius(i)).sum::<f32>() / ids.len() as f32;

    let root = radius("r");
    let level1 = mean_r(&["a", "b"]);
    let level3 = mean_r(&["g", "h"]);

    // The root sits near the centre; the deepest leaves orbit much farther out
    // — a real radial spread, not a blob.
    assert!(root < 150.0, "root near the centre, got {root}");
    assert!(
        level3 > level1,
        "deeper nodes orbit farther out: depth3 {level3} > depth1 {level1}",
    );
    assert!(
        level3 > 200.0,
        "the graph actually spreads, depth3 r={level3}"
    );

    // The isolated node is contained near the centre, not stranded at infinity
    // (the old y-only model left its x unconstrained → it escaped).
    assert!(
        radius("z") < 600.0,
        "disconnected node tethered near centre, got {}",
        radius("z"),
    );
}

#[test]
fn drag_pin_then_release_resumes_and_resettles() {
    let g = layered_graph();
    let fd = ForceDirected::default();
    let mut engine = fd.build_engine(&g, PhysicsConfig::default());
    for _ in 0..300 {
        if engine.step().settled {
            break;
        }
    }
    assert!(engine.settled());

    // Pin a node far away (a user drag) → sim resumes.
    engine.pin("g", 999.0, -999.0);
    assert!(!engine.settled(), "pin resumed the sim");
    let s = engine.step();
    assert!(s.max_speed >= 0.0);

    // Release and let it resettle.
    engine.release("g");
    for _ in 0..300 {
        if engine.step().settled {
            break;
        }
    }
    assert!(engine.settled(), "resettled after release");
}

/// Build an N-node synthetic graph: a spread of warm positions + a sparse edge
/// set (each node calls two earlier nodes) so springs + repulsion + gravity all
/// fire — the worst case the perf budget targets.
fn synthetic_graph(n: usize) -> WorkspaceGraph {
    let nodes: Vec<Node> = (0..n).map(|i| node(&format!("n{i}"))).collect();
    let mut edges = Vec::with_capacity(n * 2);
    for i in 1..n {
        edges.push(calls(&format!("n{i}"), &format!("n{}", i / 2)));
        if i > 3 {
            edges.push(calls(&format!("n{i}"), &format!("n{}", i / 3)));
        }
    }
    WorkspaceGraph {
        nodes,
        edges,
        clusters: vec![],
    }
}

#[test]
fn perf_1500_node_step_within_settle_budget() {
    const N: usize = 1500;
    let g = synthetic_graph(N);
    // Slightly wider warm spacing so the first frames aren't all coincident.
    let fd = ForceDirected::new(LayoutConfig {
        warm_x_spacing: 14.0,
        ..Default::default()
    });
    let mut engine = fd.build_engine(&g, PhysicsConfig::default());

    // Warm the allocation (first step builds the quadtree / scratch buffers).
    engine.step();

    // Median of several active steps (the sim won't settle in this window, so
    // every step does the full force pass — the worst case).
    let mut times = Vec::new();
    for _ in 0..9 {
        let t = Instant::now();
        engine.step();
        times.push(t.elapsed());
    }
    times.sort();
    let median = times[times.len() / 2];
    eprintln!(
        "[perf] {N}-node Barnes-Hut step: median {:?} (min {:?}, max {:?})",
        median,
        times.first().unwrap(),
        times.last().unwrap(),
    );

    if cfg!(debug_assertions) {
        // Debug builds are ~10-30× slower than release; just guard against a
        // pathological blow-up. The real budget is checked in release.
        assert!(
            median.as_millis() < 150,
            "debug step {median:?} — sanity ceiling",
        );
    } else {
        // Plan v2 §2.5: < 10 ms during settle for N ≤ 1500.
        assert!(
            median.as_millis() < 10,
            "release step {median:?} exceeds the 10ms settle budget",
        );
    }
}

#[test]
fn perf_frozen_graph_step_is_trivial() {
    // Steady-state budget: < 5 ms when frozen. A settled engine's step is a
    // flag check, so this is comfortably met regardless of N. A single node at
    // its y-target settles in a couple of frames; then 1000 frozen steps cost
    // essentially nothing.
    let mut engine = ForceDirected::default().build_engine(
        &WorkspaceGraph {
            nodes: vec![node("solo")],
            edges: vec![],
            clusters: vec![],
        },
        PhysicsConfig::default(),
    );
    for _ in 0..50 {
        if engine.step().settled {
            break;
        }
    }
    assert!(engine.settled(), "trivial graph froze");

    let t = Instant::now();
    for _ in 0..1000 {
        engine.step();
    }
    let per = t.elapsed() / 1000;
    eprintln!("[perf] frozen step: {per:?} (×1000)");
    assert!(per.as_micros() < 5_000, "frozen step {per:?} > 5ms");
}
