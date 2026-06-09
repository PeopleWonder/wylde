//! The **stable-grid** layout backend (Build Order §4 `graph/layout/
//! stable_grid.rs`) — a top-level *service* grid. Services (crates / top-level
//! path components) get deterministic grid cells so their on-screen positions
//! are memorisable: "wylde-harness is always over here." Within a cell, the
//! service's own nodes pack in a compact circle.
//!
//! This is the most stable of the three layouts (Plan v2 §7.1: "stable
//! positions at workspace-root level") — built for navigation muscle memory.
//!
//! ## Algorithm
//!
//! 1. **Service of each node** = the first path component of its `file`
//!    (`wylde-harness/src/foo.rs` → `wylde-harness`). File-less nodes fall into
//!    a synthetic `""` service.
//! 2. **Assign cells by hash.** Each service has a *preferred* cell from a hash
//!    of its name; collisions resolve by deterministic linear probing on a grid
//!    sized to hold the service set. For a fixed service set the mapping is
//!    stable across reloads (the muscle-memory guarantee). *Caveat:* adding or
//!    removing a service can resize the grid and reshuffle cells — full
//!    cross-set cell pinning is a C-settings refinement.
//! 3. **Pack** each service's nodes in a compact circle on its cell centre.
//!
//! Pure + deterministic.

use std::collections::{BTreeMap, HashMap};

use crate::graph::model::{Layout, Position, WorkspaceGraph};

use super::config::StableGridConfig;
use super::pack::circle_pack;
use super::LayoutBackend;

/// The stable-grid backend. Holds its geometry knobs.
#[derive(Clone, Copy, Debug, Default)]
pub struct StableGrid {
    pub cfg: StableGridConfig,
}

impl StableGrid {
    pub fn new(cfg: StableGridConfig) -> Self {
        Self { cfg }
    }
}

/// The service a node belongs to: the first path component of its `file`
/// (accepting `/` and `\`). File-less nodes map to the synthetic `""` service.
fn service_of(file: &str) -> String {
    let norm = file.replace('\\', "/");
    norm.split('/')
        .find(|c| !c.is_empty())
        .unwrap_or("")
        .to_owned()
}

/// Stable FNV-1a hash (matches `render::style`'s palette hash) — deterministic
/// across runs, unlike `DefaultHasher`.
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    h
}

impl StableGrid {
    /// Assign each service a grid cell index. Deterministic: services take their
    /// hash-preferred cell, with forward linear probing (wrapping) resolving
    /// collisions in a stable order. Returns `service → (row, col)`.
    fn assign_cells(&self, services: &[String]) -> HashMap<String, (usize, usize)> {
        let cols = self.cfg.grid_cols.max(1);
        let count = services.len();
        if count == 0 {
            return HashMap::new();
        }
        let rows = count.div_ceil(cols);
        let capacity = rows * cols;

        // Assign in a stable order independent of input ordering: by
        // (preferred cell, name). This makes probing outcomes reproducible.
        let mut order: Vec<&String> = services.iter().collect();
        order.sort_by_key(|s| (fnv1a(s) as usize % capacity, (*s).clone()));

        let mut occupied = vec![false; capacity];
        let mut cells = HashMap::with_capacity(count);
        for s in order {
            let mut idx = fnv1a(s) as usize % capacity;
            while occupied[idx] {
                idx = (idx + 1) % capacity;
            }
            occupied[idx] = true;
            cells.insert(s.clone(), (idx / cols, idx % cols));
        }
        cells
    }

    /// The model-space centre of grid cell `(row, col)`, with the whole grid
    /// centred on the origin.
    fn cell_centre(&self, row: usize, col: usize, rows: usize, cols: usize) -> Position {
        let x = (col as f32 - (cols as f32 - 1.0) * 0.5) * self.cfg.cell_size;
        let y = (row as f32 - (rows as f32 - 1.0) * 0.5) * self.cfg.cell_size;
        Position { x, y, z: 0.0 }
    }
}

impl LayoutBackend for StableGrid {
    fn name(&self) -> &'static str {
        "stable_grid"
    }

    fn compute_positions(&self, graph: &WorkspaceGraph) -> Layout {
        if graph.nodes.is_empty() {
            return Layout::default();
        }

        // Group node ids by service (sorted ids for a deterministic pack).
        let mut by_service: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for node in &graph.nodes {
            by_service
                .entry(service_of(&node.file))
                .or_default()
                .push(node.id.clone());
        }
        for ids in by_service.values_mut() {
            ids.sort();
            ids.dedup();
        }

        let services: Vec<String> = by_service.keys().cloned().collect();
        let cols = self.cfg.grid_cols.max(1);
        let rows = services.len().div_ceil(cols);
        let cells = self.assign_cells(&services);

        let mut positions: HashMap<String, Position> = HashMap::with_capacity(graph.nodes.len());
        for (service, ids) in &by_service {
            let (r, c) = cells.get(service).copied().unwrap_or((0, 0));
            let centre = self.cell_centre(r, c, rows, cols);
            for (id, p) in circle_pack(centre.x, centre.y, self.cfg.intra_spacing, ids.clone()) {
                positions.insert(id, p);
            }
        }

        Layout::from_positions(positions)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::model::{Node, NodeKind};

    fn node(id: &str, file: &str) -> Node {
        Node {
            id: id.to_owned(),
            kind: NodeKind::Function,
            name: id.to_owned(),
            file: file.to_owned(),
            line: 0,
            position: Position::default(),
            style: Default::default(),
        }
    }

    fn graph(nodes: Vec<Node>) -> WorkspaceGraph {
        WorkspaceGraph {
            nodes,
            edges: vec![],
            clusters: vec![],
        }
    }

    #[test]
    fn service_of_takes_first_path_component() {
        assert_eq!(service_of("wylde-harness/src/foo.rs"), "wylde-harness");
        assert_eq!(
            service_of("wylde-workspaces\\src\\lib.rs"),
            "wylde-workspaces"
        );
        assert_eq!(service_of("/leading/slash.rs"), "leading");
        // A root-level file (no directory) is its own service — the filename.
        assert_eq!(service_of("top.rs"), "top.rs");
        assert_eq!(service_of(""), "");
    }

    #[test]
    fn places_every_node() {
        let g = graph(vec![
            node("a", "svc-one/src/a.rs"),
            node("b", "svc-one/src/b.rs"),
            node("c", "svc-two/src/c.rs"),
            node("ext", ""),
        ]);
        let layout = StableGrid::default().compute_positions(&g);
        assert_eq!(layout.len(), 4);
        for id in ["a", "b", "c", "ext"] {
            assert!(layout.get(id).is_some(), "{id} placed");
            assert_eq!(layout.get(id).unwrap().z, 0.0);
        }
    }

    #[test]
    fn deterministic_same_workspace_same_grid() {
        let mk = || {
            graph(vec![
                node("h1", "wylde-harness/src/a.rs"),
                node("w1", "wylde-workspaces/src/b.rs"),
                node("t1", "wylde-treesitter/src/c.rs"),
                node("h2", "wylde-harness/src/d.rs"),
            ])
        };
        let l1 = StableGrid::default().compute_positions(&mk());
        let l2 = StableGrid::default().compute_positions(&mk());
        for id in ["h1", "w1", "t1", "h2"] {
            assert_eq!(l1.get(id), l2.get(id), "{id} stable across reloads");
        }
    }

    #[test]
    fn nodes_in_same_service_cluster_together() {
        // Two harness nodes should land near each other; far from a different
        // service's node.
        let g = graph(vec![
            node("h1", "wylde-harness/src/a.rs"),
            node("h2", "wylde-harness/src/b.rs"),
            node("w1", "wylde-workspaces/src/c.rs"),
        ]);
        let layout = StableGrid::default().compute_positions(&g);
        let h1 = layout.get("h1").unwrap();
        let h2 = layout.get("h2").unwrap();
        let w1 = layout.get("w1").unwrap();
        let intra = ((h1.x - h2.x).powi(2) + (h1.y - h2.y).powi(2)).sqrt();
        let inter = ((h1.x - w1.x).powi(2) + (h1.y - w1.y).powi(2)).sqrt();
        assert!(
            intra < inter,
            "same-service spacing {intra} < cross-service spacing {inter}"
        );
    }

    #[test]
    fn services_occupy_distinct_cells() {
        let g = graph(vec![
            node("a", "s1/a.rs"),
            node("b", "s2/b.rs"),
            node("c", "s3/c.rs"),
            node("d", "s4/d.rs"),
            node("e", "s5/e.rs"),
        ]);
        let sg = StableGrid::default();
        let services: Vec<String> = ["s1", "s2", "s3", "s4", "s5"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let cells = sg.assign_cells(&services);
        let distinct: std::collections::HashSet<_> = cells.values().copied().collect();
        assert_eq!(distinct.len(), 5, "no two services share a cell");
        let _ = g;
    }

    #[test]
    fn empty_graph_is_empty_layout() {
        assert!(StableGrid::default()
            .compute_positions(&graph(vec![]))
            .is_empty());
    }

    #[test]
    fn name_is_stable_grid() {
        assert_eq!(StableGrid::default().name(), "stable_grid");
    }
}
