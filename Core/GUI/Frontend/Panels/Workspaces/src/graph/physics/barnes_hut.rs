//! Barnes-Hut quadtree for **O(N log N)** Coulomb repulsion (Plan v2 §7.10).
//!
//! Naive all-pairs repulsion is O(N²): at N = 1500 that's ~2.25 M pair
//! evaluations per frame, which blows the < 10 ms settle budget (§2.5). The
//! quadtree groups distant nodes into aggregate masses: when a cell is far
//! enough away (`cell_size / distance < θ`) its N members are treated as one
//! body at their centre of mass, collapsing the cost to O(N log N).
//!
//! 2D only — the tree partitions `(x, y)`; `z` stays 0 in v1 (a v2 3D renderer
//! would swap this for an octree). The exact-descent path (`θ = 0`) reproduces
//! the naive pairwise sum (minus self), which the approximation-error test
//! pins against [`naive_force`].

use super::config::PhysicsConfig;
use super::forces::coulomb;

/// One quadtree cell. A cell is *empty* (`count == 0`), a *leaf* (one body,
/// `children == None`), or *internal* (`children` populated, `count` = total
/// bodies below it at their centre of mass).
#[derive(Clone, Copy, Debug)]
struct Cell {
    // Square region this cell covers (model px).
    x0: f32,
    y0: f32,
    size: f32,
    // Aggregate of all bodies under this cell.
    count: f32,
    com_x: f32,
    com_y: f32,
    // A single body if this is a leaf: (body index, x, y).
    body: Option<(u32, f32, f32)>,
    // Four child cell indices (NW, NE, SW, SE) if internal.
    children: Option<[u32; 4]>,
}

impl Cell {
    fn empty(x0: f32, y0: f32, size: f32) -> Self {
        Cell {
            x0,
            y0,
            size,
            count: 0.0,
            com_x: 0.0,
            com_y: 0.0,
            body: None,
            children: None,
        }
    }
}

/// A built quadtree over a fixed set of body positions. Rebuilt each frame —
/// build is O(N log N) and the engine reuses the allocation across frames.
pub struct QuadTree {
    cells: Vec<Cell>,
}

/// Subdivision guard: coincident or near-coincident bodies would otherwise
/// recurse forever. At this depth a leaf just accumulates into the aggregate
/// (the force on such tight clusters is min-distance-floored anyway).
const MAX_DEPTH: u32 = 24;

impl QuadTree {
    /// Build a tree over `positions` (body `i` is index `i`). Empty input
    /// yields an empty tree whose [`force_on`](Self::force_on) is always zero.
    pub fn build(positions: &[(f32, f32)]) -> Self {
        let mut cells = Vec::with_capacity(positions.len() * 2 + 1);
        if positions.is_empty() {
            return QuadTree { cells };
        }

        // Bounding square covering every body, padded so points on the edge
        // sit strictly inside.
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
        for &(x, y) in positions {
            min_x = min_x.min(x);
            min_y = min_y.min(y);
            max_x = max_x.max(x);
            max_y = max_y.max(y);
        }
        let span = (max_x - min_x).max(max_y - min_y).max(1.0) * 1.01;

        cells.push(Cell::empty(
            min_x - 0.005 * span,
            min_y - 0.005 * span,
            span,
        ));
        let mut tree = QuadTree { cells };
        for (i, &(x, y)) in positions.iter().enumerate() {
            tree.insert(0, i as u32, x, y, 0);
        }
        tree
    }

    /// Insert body `idx` at `(x, y)` into the subtree rooted at `cell`.
    fn insert(&mut self, cell: usize, idx: u32, x: f32, y: f32, depth: u32) {
        // Fold into the running aggregate (centre of mass) regardless of leaf
        // vs internal.
        let c = &mut self.cells[cell];
        let new_count = c.count + 1.0;
        c.com_x = (c.com_x * c.count + x) / new_count;
        c.com_y = (c.com_y * c.count + y) / new_count;
        c.count = new_count;

        match (c.body, c.children) {
            // Empty cell → becomes a leaf.
            (None, None) if c.count == 1.0 => {
                self.cells[cell].body = Some((idx, x, y));
            }
            // Occupied leaf → subdivide, push the existing body down, then us.
            (Some((bi, bx, by)), None) => {
                if depth >= MAX_DEPTH {
                    // Give up subdividing coincident points; the aggregate
                    // already counts both. Keep the original as the leaf body.
                    return;
                }
                self.subdivide(cell);
                self.cells[cell].body = None;
                self.place_in_child(cell, bi, bx, by, depth);
                self.place_in_child(cell, idx, x, y, depth);
            }
            // Internal cell → recurse into the right child.
            (None, Some(_)) => {
                self.place_in_child(cell, idx, x, y, depth);
            }
            // (None, None) with count>1 only happens at MAX_DEPTH bail-out.
            _ => {}
        }
    }

    /// Create four empty child cells for `cell`.
    fn subdivide(&mut self, cell: usize) {
        let (x0, y0, half) = {
            let c = &self.cells[cell];
            (c.x0, c.y0, c.size * 0.5)
        };
        let base = self.cells.len() as u32;
        // NW, NE, SW, SE.
        self.cells.push(Cell::empty(x0, y0, half));
        self.cells.push(Cell::empty(x0 + half, y0, half));
        self.cells.push(Cell::empty(x0, y0 + half, half));
        self.cells.push(Cell::empty(x0 + half, y0 + half, half));
        self.cells[cell].children = Some([base, base + 1, base + 2, base + 3]);
    }

    /// Route `(x, y)` into the correct quadrant child of `cell`.
    fn place_in_child(&mut self, cell: usize, idx: u32, x: f32, y: f32, depth: u32) {
        let (children, x0, y0, half) = {
            let c = &self.cells[cell];
            (c.children.unwrap(), c.x0, c.y0, c.size * 0.5) // INVARIANT: place_in_child is only called on a subdivided (internal) cell, whose children are Some. wylde-check: panel-panic-allowed
        };
        let east = x >= x0 + half;
        let south = y >= y0 + half;
        let quadrant = (south as usize) * 2 + (east as usize);
        self.insert(children[quadrant] as usize, idx, x, y, depth + 1);
    }

    /// Total repulsion on the body at `(qx, qy)` (index `self_idx`) from every
    /// other body, via the Barnes-Hut approximation. With `cfg.theta == 0` this
    /// degrades to an exact pairwise sum (minus self).
    pub fn force_on(&self, qx: f32, qy: f32, self_idx: u32, cfg: &PhysicsConfig) -> (f32, f32) {
        if self.cells.is_empty() {
            return (0.0, 0.0);
        }
        let mut acc = (0.0_f32, 0.0_f32);
        let mut stack = vec![0usize];
        while let Some(ci) = stack.pop() {
            let c = self.cells[ci];
            if c.count == 0.0 {
                continue;
            }
            // Leaf with a single body.
            if let (Some((bi, bx, by)), None) = (c.body, c.children) {
                if bi == self_idx {
                    continue; // never repel from self
                }
                let (fx, fy) = coulomb(
                    qx - bx,
                    qy - by,
                    1.0,
                    cfg.repulsion_strength,
                    cfg.min_distance,
                    cfg.cutoff_radius,
                );
                acc.0 += fx;
                acc.1 += fy;
                continue;
            }
            // Internal cell: aggregate if far enough, else descend.
            let dx = qx - c.com_x;
            let dy = qy - c.com_y;
            let dist = (dx * dx + dy * dy).sqrt();
            if dist > 0.0 && c.size / dist < cfg.theta {
                let (fx, fy) = coulomb(
                    dx,
                    dy,
                    c.count,
                    cfg.repulsion_strength,
                    cfg.min_distance,
                    cfg.cutoff_radius,
                );
                acc.0 += fx;
                acc.1 += fy;
            } else if let Some(children) = c.children {
                stack.extend(children.iter().map(|&c| c as usize));
            }
        }
        acc
    }

    /// Number of cells allocated — exposed for the quadtree-shape test.
    pub fn cell_count(&self) -> usize {
        self.cells.len()
    }
}

/// Exact O(N²) repulsion on body `idx` — the reference the Barnes-Hut
/// approximation is checked against in tests (and never used in the hot path).
pub fn naive_force(positions: &[(f32, f32)], idx: usize, cfg: &PhysicsConfig) -> (f32, f32) {
    let (qx, qy) = positions[idx];
    let mut acc = (0.0_f32, 0.0_f32);
    for (j, &(bx, by)) in positions.iter().enumerate() {
        if j == idx {
            continue;
        }
        let (fx, fy) = coulomb(
            qx - bx,
            qy - by,
            1.0,
            cfg.repulsion_strength,
            cfg.min_distance,
            cfg.cutoff_radius,
        );
        acc.0 += fx;
        acc.1 += fy;
    }
    acc
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(theta: f32) -> PhysicsConfig {
        PhysicsConfig {
            theta,
            // Wide cutoff so the approximation test sees every body.
            cutoff_radius: 100_000.0,
            ..Default::default()
        }
    }

    #[test]
    fn empty_tree_has_no_force() {
        let t = QuadTree::build(&[]);
        assert_eq!(t.force_on(0.0, 0.0, 0, &cfg(0.85)), (0.0, 0.0));
        assert_eq!(t.cell_count(), 0);
    }

    #[test]
    fn single_body_only_repels_others_not_itself() {
        let pts = vec![(0.0, 0.0)];
        let t = QuadTree::build(&pts);
        // Querying as the body itself → no self force.
        assert_eq!(t.force_on(0.0, 0.0, 0, &cfg(0.85)), (0.0, 0.0));
        // Querying as a different body nearby → pushed away.
        let (fx, _) = t.force_on(10.0, 0.0, 1, &cfg(0.85));
        assert!(fx > 0.0);
    }

    #[test]
    fn tree_subdivides_into_quadrants() {
        // Four points, one per quadrant of a centred square.
        let pts = vec![(-10.0, -10.0), (10.0, -10.0), (-10.0, 10.0), (10.0, 10.0)];
        let t = QuadTree::build(&pts);
        // Root + at least the four quadrant children.
        assert!(t.cell_count() >= 5, "got {}", t.cell_count());
    }

    #[test]
    fn exact_descent_matches_naive_within_bounds() {
        // ≤20 nodes, θ = 0 forces full descent → must equal the naive sum.
        let pts: Vec<(f32, f32)> = (0..20)
            .map(|i| {
                let a = i as f32 * 0.7;
                (40.0 * a.cos() + i as f32, 35.0 * a.sin() - i as f32)
            })
            .collect();
        let t = QuadTree::build(&pts);
        let c = cfg(0.0);
        for i in 0..pts.len() {
            let (bx, by) = pts[i];
            let bh = t.force_on(bx, by, i as u32, &c);
            let nv = naive_force(&pts, i, &c);
            assert!(
                (bh.0 - nv.0).abs() < 1e-2 && (bh.1 - nv.1).abs() < 1e-2,
                "body {i}: bh={bh:?} naive={nv:?}",
            );
        }
    }

    #[test]
    fn approximation_error_bounded_vs_naive() {
        // With a normal θ the BH force should still be close to naive on a
        // small graph (the aggregation error is bounded, not arbitrary).
        let pts: Vec<(f32, f32)> = (0..20)
            .map(|i| {
                let a = i as f32 * 1.3;
                (60.0 * a.cos(), 60.0 * a.sin())
            })
            .collect();
        let t = QuadTree::build(&pts);
        let c = cfg(0.85);
        for i in 0..pts.len() {
            let (bx, by) = pts[i];
            let bh = t.force_on(bx, by, i as u32, &c);
            let nv = naive_force(&pts, i, &c);
            let err = ((bh.0 - nv.0).powi(2) + (bh.1 - nv.1).powi(2)).sqrt();
            let mag = (nv.0 * nv.0 + nv.1 * nv.1).sqrt().max(1e-6);
            // Relative error under 25% — comfortably bounded for θ = 0.85.
            assert!(err / mag < 0.25, "body {i}: rel err {}", err / mag);
        }
    }

    #[test]
    fn cutoff_zeroes_far_bodies() {
        // Two bodies far apart, cutoff between them → no force.
        let pts = vec![(0.0, 0.0), (1000.0, 0.0)];
        let t = QuadTree::build(&pts);
        let c = PhysicsConfig {
            theta: 0.85,
            cutoff_radius: 200.0,
            ..Default::default()
        };
        assert_eq!(t.force_on(0.0, 0.0, 0, &c), (0.0, 0.0));
    }

    #[test]
    fn coincident_points_do_not_hang_or_nan() {
        // Many bodies at the same spot exercise the MAX_DEPTH bail-out.
        let pts = vec![(5.0, 5.0); 50];
        let t = QuadTree::build(&pts);
        let (fx, fy) = t.force_on(5.0, 5.0, 0, &cfg(0.85));
        assert!(fx.is_finite() && fy.is_finite());
    }
}
