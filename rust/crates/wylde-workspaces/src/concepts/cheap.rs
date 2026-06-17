//! Phase-0 "cheap concepts": label the EXISTING directory clusters
//! ([`crate::graph::projection::cluster_by_dir`]) into stand-in [`Concept`]s
//! (thesis §7 Phase 0 / S0.2).
//!
//! This proves the whole concept pipeline end-to-end with near-zero new
//! clustering code: take the structural `cluster_by_dir` partition the
//! `workspaces.graph` verb already returns, derive a human label + description
//! per directory, infer a `CHILD_OF` parent from the directory tree, and emit
//! `Concept` records. Everything downstream (the Concepts sub-tab, reverse
//! lookup, hybrid search) can then be built and tested against a *real* (if
//! coarse) concept set before Phase-2 semantic clustering replaces the source.
//!
//! ## Labeling is deterministic here, LLM later (sequencing decision)
//!
//! The thesis envisions an LLM naming each cluster. To keep Phase 0 **banked,
//! offline, and unit-testable** (the build verb must be green via `cargo test`
//! with no Ollama/Neo4j), [`label_for_dir`] derives the label/description from
//! the directory path *heuristically*. LLM labeling at scale is Phase 2 (S2.2),
//! where it is fail-soft and runs over the semantic clusters that actually
//! warrant prose. [`build_concepts`] is pure so both callers — the offline test
//! and the future LLM-enriched verb — share one deterministic skeleton.

use std::collections::BTreeMap;

use super::concept::{Concept, ConceptSource};
use crate::graph::projection::{Cluster, WorkspaceGraph};

/// The id prefix that marks a directory-derived (Phase-0) concept. Lets a
/// later phase find + replace exactly the stand-ins without disturbing
/// manually-authored or semantic concepts.
pub const DIR_CONCEPT_PREFIX: &str = "dir:";

/// Build the Phase-0 concept set from a workspace graph. Pure + deterministic
/// (sorted, no clock beyond the per-concept stamp `Concept::new` adds): one
/// concept per directory cluster, with members, representative files, and an
/// inferred directory-tree `CHILD_OF` parent.
pub fn build_concepts(graph: &WorkspaceGraph) -> Vec<Concept> {
    // file lookup: node id → its file (for member_files).
    let file_of: BTreeMap<&str, &str> = graph
        .nodes
        .iter()
        .filter(|n| !n.file.as_os_str().is_empty())
        .map(|n| (n.id.as_str(), n.file.to_str().unwrap_or_default()))
        .collect();

    // Index clusters by their dir key so parent inference is a map lookup.
    let dir_keys: Vec<&str> = graph.clusters.iter().map(|c| c.id.as_str()).collect();

    let mut out: Vec<Concept> = graph
        .clusters
        .iter()
        .map(|cluster| concept_for_cluster(cluster, &file_of, &dir_keys))
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// One concept from one directory cluster.
fn concept_for_cluster(
    cluster: &Cluster,
    file_of: &BTreeMap<&str, &str>,
    dir_keys: &[&str],
) -> Concept {
    let (label, description) = label_for_dir(&cluster.parent_breadcrumb, cluster.member_ids.len());

    let mut concept = Concept::new(
        format!("{DIR_CONCEPT_PREFIX}{}", cluster.id),
        label,
        description,
        ConceptSource::DirectoryCluster,
    );

    // Members = the cluster's nodes; member_files = their distinct files (sorted).
    concept.members = cluster.member_ids.clone();
    let mut files: Vec<String> = cluster
        .member_ids
        .iter()
        .filter_map(|m| file_of.get(m.as_str()).map(|f| f.to_string()))
        .collect();
    files.sort();
    files.dedup();
    concept.member_files = files;

    // CHILD_OF: the nearest ancestor directory that is *also* a cluster.
    if let Some(parent_dir) = nearest_ancestor_cluster(&cluster.id, dir_keys) {
        concept.parent_concepts = vec![format!("{DIR_CONCEPT_PREFIX}{parent_dir}")];
    }
    concept
}

/// Derive a `(label, description)` from a directory breadcrumb (heuristic; see
/// the module note on the LLM-later sequencing). The label is the humanised
/// last path component; the description names the subtree + its size.
pub fn label_for_dir(breadcrumb: &[String], member_count: usize) -> (String, String) {
    let leaf = breadcrumb.last().map(String::as_str).unwrap_or("root");
    let label = humanize(leaf);
    let path = if breadcrumb.is_empty() {
        "the workspace root".to_owned()
    } else {
        format!("`{}`", breadcrumb.join("/"))
    };
    let noun = if member_count == 1 { "symbol" } else { "symbols" };
    let description = format!(
        "Code under {path} ({member_count} {noun}). Directory-derived stand-in concept (Phase 0); \
         a semantic re-clustering pass will refine this."
    );
    (label, description)
}

/// Humanise a directory segment into a label: split on `_`/`-`, title-case each
/// word. `graph_writer` → `Graph Writer`; `rag` → `Rag`.
fn humanize(seg: &str) -> String {
    let words: Vec<String> = seg
        .split(['_', '-', ' '])
        .filter(|w| !w.is_empty())
        .map(|w| {
            let mut chars = w.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect();
    if words.is_empty() {
        seg.to_owned()
    } else {
        words.join(" ")
    }
}

/// The longest cluster dir key that is a strict path-prefix ancestor of `dir`.
/// Returns `None` when no shallower cluster contains it. Uses path-component
/// boundaries so `src/g` is *not* treated as an ancestor of `src/graph`.
fn nearest_ancestor_cluster<'a>(dir: &str, dir_keys: &[&'a str]) -> Option<&'a str> {
    dir_keys
        .iter()
        .copied()
        .filter(|cand| *cand != dir && is_path_ancestor(cand, dir))
        // longest (closest) ancestor wins.
        .max_by_key(|cand| cand.len())
}

/// True iff `ancestor` is a proper path-prefix of `descendant` on a component
/// boundary (handles both `/` and `\` separators).
fn is_path_ancestor(ancestor: &str, descendant: &str) -> bool {
    if !descendant.starts_with(ancestor) {
        return false;
    }
    match descendant[ancestor.len()..].chars().next() {
        Some(c) => c == '/' || c == '\\',
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::projection::{Cluster, Node, NodeKind, NodeStyle, Position, WorkspaceGraph};
    use std::path::PathBuf;

    fn node(id: &str, file: &str) -> Node {
        Node {
            id: id.to_owned(),
            kind: NodeKind::Function,
            name: id.to_owned(),
            file: PathBuf::from(file),
            line: 0,
            position: Position::default(),
            style: NodeStyle::default(),
        }
    }
    fn cluster(id: &str, members: &[&str], breadcrumb: &[&str]) -> Cluster {
        Cluster {
            id: id.to_owned(),
            member_ids: members.iter().map(|s| s.to_string()).collect(),
            parent_breadcrumb: breadcrumb.iter().map(|s| s.to_string()).collect(),
            zoom_threshold: 1.0,
        }
    }

    fn sample_graph() -> WorkspaceGraph {
        WorkspaceGraph {
            nodes: vec![
                node("alpha", "src/graph/api.rs"),
                node("beta", "src/graph/api.rs"),
                node("gamma", "src/graph/cluster/mod.rs"),
                node("delta", "src/rag/search.rs"),
            ],
            edges: vec![],
            clusters: vec![
                cluster("src/graph", &["alpha", "beta"], &["src", "graph"]),
                cluster("src/graph/cluster", &["gamma"], &["src", "graph", "cluster"]),
                cluster("src/rag", &["delta"], &["src", "rag"]),
            ],
        }
    }

    #[test]
    fn builds_one_concept_per_cluster_with_dir_prefix() {
        let cs = build_concepts(&sample_graph());
        assert_eq!(cs.len(), 3);
        let ids: Vec<&str> = cs.iter().map(|c| c.id.as_str()).collect();
        assert!(ids.iter().all(|id| id.starts_with("dir:")));
        assert!(ids.contains(&"dir:src/graph"));
    }

    #[test]
    fn members_and_files_are_carried() {
        let cs = build_concepts(&sample_graph());
        let graph_c = cs.iter().find(|c| c.id == "dir:src/graph").unwrap();
        assert_eq!(graph_c.members, vec!["alpha", "beta"]);
        assert_eq!(graph_c.member_files, vec!["src/graph/api.rs"], "deduped files");
    }

    #[test]
    fn labels_are_humanised() {
        let (label, desc) = label_for_dir(&["src".into(), "graph_writer".into()], 5);
        assert_eq!(label, "Graph Writer");
        assert!(desc.contains("`src/graph_writer`"));
        assert!(desc.contains("5 symbols"));
        // singular noun for one member.
        let (_, d1) = label_for_dir(&["x".into()], 1);
        assert!(d1.contains("1 symbol)"), "{d1}");
    }

    #[test]
    fn child_of_links_to_nearest_ancestor_cluster() {
        let cs = build_concepts(&sample_graph());
        let cluster_c = cs.iter().find(|c| c.id == "dir:src/graph/cluster").unwrap();
        // src/graph/cluster is under src/graph (the nearest ancestor cluster),
        // NOT under src/rag.
        assert_eq!(cluster_c.parent_concepts, vec!["dir:src/graph"]);
        // src/graph has no ancestor cluster (src itself isn't a cluster here).
        let graph_c = cs.iter().find(|c| c.id == "dir:src/graph").unwrap();
        assert!(graph_c.parent_concepts.is_empty());
    }

    #[test]
    fn path_ancestor_respects_component_boundaries() {
        assert!(is_path_ancestor("src/graph", "src/graph/cluster"));
        assert!(is_path_ancestor("src\\graph", "src\\graph\\cluster"));
        // prefix-but-not-component-boundary must NOT match.
        assert!(!is_path_ancestor("src/g", "src/graph"));
        assert!(!is_path_ancestor("src/graph", "src/graph"));
    }

    #[test]
    fn directory_concepts_have_no_centroid() {
        let cs = build_concepts(&sample_graph());
        assert!(cs.iter().all(|c| c.centroid.is_none()));
        assert!(cs.iter().all(|c| c.source == ConceptSource::DirectoryCluster));
    }

    #[test]
    fn empty_graph_yields_no_concepts() {
        assert!(build_concepts(&WorkspaceGraph::default()).is_empty());
    }

    #[test]
    fn output_is_deterministic_and_sorted() {
        let g = sample_graph();
        let a = build_concepts(&g);
        let b = build_concepts(&g);
        // ids sorted ascending.
        let ids: Vec<&str> = a.iter().map(|c| c.id.as_str()).collect();
        let mut sorted = ids.clone();
        sorted.sort();
        assert_eq!(ids, sorted);
        // Structural determinism (timestamps from `Concept::new` are expected to
        // differ across calls, so compare everything *but* the stamps).
        let strip = |cs: &[Concept]| -> Vec<(String, String, Vec<String>, Vec<String>)> {
            cs.iter()
                .map(|c| (c.id.clone(), c.label.clone(), c.members.clone(), c.parent_concepts.clone()))
                .collect()
        };
        assert_eq!(strip(&a), strip(&b));
    }
}
