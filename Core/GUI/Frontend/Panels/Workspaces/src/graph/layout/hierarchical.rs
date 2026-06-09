//! The **hierarchical** layout backend (Build Order §4 `graph/layout/
//! hierarchical.rs`) — a module-grouped tree. Deterministic, compact, no
//! physics: the module hierarchy is laid out as a tidy tree and each module's
//! own nodes are packed in a compact circle on the module's tree position.
//!
//! This is the "module-grouped tree as alternate" of Plan v2 OQ1 and the
//! deterministic half of §7.1's visual model (stable structure at the
//! module level; the *intra-module* spread is the compact pack, not physics —
//! force-directed-within-modules is a later refinement, not v1).
//!
//! ## Algorithm
//!
//! 1. **Group by module.** A node's module is its `file`'s parent directory
//!    (the same module proxy Slice B clusters on). File-less nodes (external
//!    edge targets) fall into a synthetic root module.
//! 2. **Build the module forest.** Each module's parent is the longest *other*
//!    module that is a path-ancestor of it; modules with no such ancestor are
//!    roots. Gaps are tolerated (`a/b/c` with no `a/b` attaches to `a`).
//! 3. **Tidy-tree pass** (the Buchheim essence, simplified): a post-order walk
//!    assigns each leaf the next horizontal slot and centres every parent over
//!    its children — contiguous, non-overlapping subtrees with parents above.
//! 4. **Pack intra-module nodes** in a compact circle on the module position.
//!
//! Pure + deterministic: sort everything by path / id so the same graph always
//! lays out identically.

use std::collections::{BTreeMap, HashMap};

use crate::graph::model::{Layout, Position, WorkspaceGraph};

use super::config::HierarchicalConfig;
use super::pack::circle_pack;
use super::LayoutBackend;

/// The hierarchical backend. Holds its geometry knobs.
#[derive(Clone, Copy, Debug, Default)]
pub struct Hierarchical {
    pub cfg: HierarchicalConfig,
}

impl Hierarchical {
    pub fn new(cfg: HierarchicalConfig) -> Self {
        Self { cfg }
    }
}

/// The module a node belongs to: the parent directory of its `file`, with both
/// `/` and `\` accepted as separators (Windows paths come off the wire too).
/// A file with no directory component (or no file at all) maps to the synthetic
/// root module `""`.
fn module_of(file: &str) -> String {
    let norm = file.replace('\\', "/");
    match norm.rsplit_once('/') {
        Some((parent, _)) => parent.to_owned(),
        None => String::new(),
    }
}

/// The module forest: child links + roots, in a stable (sorted) order.
struct ModuleTree {
    /// Modules in a stable order (sorted).
    modules: Vec<String>,
    /// `modules` index → child indices (sorted).
    children: Vec<Vec<usize>>,
    /// Roots (modules with no parent), sorted.
    roots: Vec<usize>,
}

impl ModuleTree {
    /// Build the forest from the distinct module set.
    fn build(mut modules: Vec<String>) -> Self {
        modules.sort();
        modules.dedup();
        let n = modules.len();
        let index: HashMap<&str, usize> = modules
            .iter()
            .enumerate()
            .map(|(i, m)| (m.as_str(), i))
            .collect();

        let mut children = vec![Vec::new(); n];
        let mut roots = Vec::new();
        for (i, m) in modules.iter().enumerate() {
            // The parent is the *longest* module that is a strict ancestor —
            // walk up the path components looking for one in the set.
            match longest_ancestor(m, &index) {
                Some(p) => children[p].push(i),
                None => roots.push(i),
            }
        }
        // Stable order: children/roots already follow `modules` (sorted) order
        // because we pushed in ascending `i`.
        ModuleTree {
            modules,
            children,
            roots,
        }
    }
}

/// Find the longest module in `index` that is a strict path-ancestor of `m`
/// (excluding `m` itself). Walks `m`'s parent directories from nearest up.
fn longest_ancestor(m: &str, index: &HashMap<&str, usize>) -> Option<usize> {
    if m.is_empty() {
        return None; // the root module has no parent
    }
    let mut cur = m;
    while let Some((parent, _)) = cur.rsplit_once('/') {
        if let Some(&idx) = index.get(parent) {
            return Some(idx);
        }
        cur = parent;
    }
    // No ancestor directory is itself a module. Attach to the synthetic root
    // module `""` if it exists (file-less nodes created it); else this is a
    // forest root.
    index.get("").copied()
}

impl LayoutBackend for Hierarchical {
    fn name(&self) -> &'static str {
        "hierarchical"
    }

    fn compute_positions(&self, graph: &WorkspaceGraph) -> Layout {
        if graph.nodes.is_empty() {
            return Layout::default();
        }

        // 1. Group node ids by module (sorted ids within each module for a
        //    deterministic pack).
        let mut by_module: BTreeMap<String, Vec<String>> = BTreeMap::new();
        for node in &graph.nodes {
            by_module
                .entry(module_of(&node.file))
                .or_default()
                .push(node.id.clone());
        }
        for ids in by_module.values_mut() {
            ids.sort();
            ids.dedup();
        }

        // 2. Build the module forest.
        let tree = ModuleTree::build(by_module.keys().cloned().collect());

        // 3. Tidy-tree pass → each module's (x, y).
        let module_pos = self.tidy_layout(&tree);

        // 4. Pack each module's nodes around its tree position.
        let mut positions: HashMap<String, Position> = HashMap::with_capacity(graph.nodes.len());
        for (mi, module) in tree.modules.iter().enumerate() {
            let pos = module_pos[mi];
            let ids = by_module.get(module).cloned().unwrap_or_default();
            for (id, p) in circle_pack(pos.x, pos.y, self.cfg.intra_spacing, ids) {
                positions.insert(id, p);
            }
        }

        Layout::from_positions(positions)
    }
}

impl Hierarchical {
    /// Tidy-tree positions for every module. Post-order: leaves take successive
    /// horizontal slots; each internal module is centred over its children. y is
    /// the tree depth × `module_v_spacing`. The whole forest is laid left to
    /// right (roots processed in order), so subtrees never overlap.
    fn tidy_layout(&self, tree: &ModuleTree) -> Vec<Position> {
        let n = tree.modules.len();
        let mut pos = vec![Position::default(); n];
        // `cursor` is the next free leaf slot (in units of module_h_spacing).
        let mut cursor: f32 = 0.0;
        for &root in &tree.roots {
            self.assign(root, 0, tree, &mut cursor, &mut pos);
        }
        pos
    }

    /// Recursive post-order assignment. Returns nothing; writes into `pos`.
    fn assign(
        &self,
        m: usize,
        depth: u32,
        tree: &ModuleTree,
        cursor: &mut f32,
        pos: &mut [Position],
    ) {
        let y = depth as f32 * self.cfg.module_v_spacing;
        let kids = &tree.children[m];
        if kids.is_empty() {
            // Leaf: take the next slot, advance the cursor.
            pos[m] = Position {
                x: *cursor * self.cfg.module_h_spacing,
                y,
                z: 0.0,
            };
            *cursor += 1.0;
            return;
        }
        // Lay out children first, then centre this module over them.
        for &c in kids {
            self.assign(c, depth + 1, tree, cursor, pos);
        }
        let first = pos[*kids.first().unwrap()].x;
        let last = pos[*kids.last().unwrap()].x;
        pos[m] = Position {
            x: (first + last) * 0.5,
            y,
            z: 0.0,
        };
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
    fn module_of_extracts_parent_dir() {
        assert_eq!(module_of("a/b/c.rs"), "a/b");
        assert_eq!(module_of("a\\b\\c.rs"), "a/b");
        assert_eq!(module_of("top.rs"), "");
        assert_eq!(module_of(""), "");
    }

    #[test]
    fn nested_module_attaches_to_nearest_ancestor() {
        // a/b/c attaches to a/b when present; to a when a/b is absent (gap).
        let g = graph(vec![
            node("ab", "a/b/x.rs"),    // module "a/b"
            node("abc", "a/b/c/y.rs"), // module "a/b/c" → child of "a/b"
            node("a", "a/z.rs"),       // module "a" → parent of "a/b"
        ]);
        let layout = Hierarchical::default().compute_positions(&g);
        // Three nesting levels → three distinct y bands.
        let ya = layout.get("a").unwrap().y;
        let yab = layout.get("ab").unwrap().y;
        let yabc = layout.get("abc").unwrap().y;
        assert!(ya < yab && yab < yabc, "a < a/b < a/b/c in depth");
    }

    #[test]
    fn places_every_node() {
        let g = graph(vec![
            node("f1", "src/a.rs"),
            node("f2", "src/b.rs"),
            node("f3", "src/sub/c.rs"),
            node("ext", ""), // file-less external node
        ]);
        let layout = Hierarchical::default().compute_positions(&g);
        assert_eq!(layout.len(), 4);
        for id in ["f1", "f2", "f3", "ext"] {
            assert!(layout.get(id).is_some(), "{id} placed");
            assert_eq!(layout.get(id).unwrap().z, 0.0);
        }
    }

    #[test]
    fn children_sit_below_their_parent_module() {
        // src is the parent module; src/sub is its child (deeper path).
        let g = graph(vec![
            node("a", "src/a.rs"),        // module "src"
            node("deep", "src/sub/x.rs"), // module "src/sub" (child of src)
        ]);
        let layout = Hierarchical::default().compute_positions(&g);
        let parent_y = layout.get("a").unwrap().y;
        let child_y = layout.get("deep").unwrap().y;
        assert!(
            child_y > parent_y,
            "child module ({child_y}) below parent ({parent_y})"
        );
    }

    #[test]
    fn sibling_modules_do_not_overlap_horizontally() {
        // Two sibling leaf modules under a common parent must get distinct x.
        let g = graph(vec![
            node("p", "pkg/root.rs"),  // module "pkg"
            node("x", "pkg/one/x.rs"), // module "pkg/one"
            node("y", "pkg/two/y.rs"), // module "pkg/two"
        ]);
        let layout = Hierarchical::default().compute_positions(&g);
        let xpos = layout.get("x").unwrap().x;
        let ypos = layout.get("y").unwrap().x;
        assert!(
            (xpos - ypos).abs() > 1.0,
            "sibling modules separated horizontally: {xpos} vs {ypos}"
        );
    }

    #[test]
    fn parent_is_centred_over_its_children() {
        let g = graph(vec![
            node("p", "pkg/root.rs"),
            node("x", "pkg/one/x.rs"),
            node("y", "pkg/two/y.rs"),
        ]);
        let layout = Hierarchical::default().compute_positions(&g);
        let px = layout.get("p").unwrap().x;
        let xx = layout.get("x").unwrap().x;
        let yy = layout.get("y").unwrap().x;
        let mid = (xx + yy) * 0.5;
        assert!(
            (px - mid).abs() < 1e-3,
            "parent x {px} centred over children midpoint {mid}"
        );
    }

    #[test]
    fn deterministic() {
        let mk = || {
            graph(vec![
                node("c", "z/c.rs"),
                node("a", "z/a.rs"),
                node("b", "y/b.rs"),
            ])
        };
        let l1 = Hierarchical::default().compute_positions(&mk());
        let l2 = Hierarchical::default().compute_positions(&mk());
        for id in ["a", "b", "c"] {
            assert_eq!(l1.get(id), l2.get(id), "{id} stable");
        }
    }

    #[test]
    fn empty_graph_is_empty_layout() {
        let layout = Hierarchical::default().compute_positions(&graph(vec![]));
        assert!(layout.is_empty());
    }

    #[test]
    fn name_is_hierarchical() {
        assert_eq!(Hierarchical::default().name(), "hierarchical");
    }
}
