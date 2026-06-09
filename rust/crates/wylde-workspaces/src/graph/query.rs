//! Cypher **read** queries + decoded row types for the `workspaces.graph`
//! verb (Slice B).
//!
//! This is the read half of the graph layer; the write half lives in
//! [`super::cypher`] / [`super::bolt`] (relocated, byte-identical to the
//! harness ingest writes). The two are kept cleanly separate — nothing here
//! mutates the graph.
//!
//! ## Workspace scoping (the graph schema's deliberate shape)
//!
//! Ingest tags only **Chunk** nodes with `workspace = <workspace_id>`;
//! **Entity** nodes and the typed Entity→Entity edges are *global-by-name*
//! (a function `foo` is one node regardless of workspace — see
//! [`super::cypher::UPSERT_ENTITIES`] and the `graph_writer` module docs). So
//! "the workspace's graph" is recovered by traversal, not a node/edge tag:
//!
//!   * **Nodes** = every `Entity` `MENTIONED_IN` one of the workspace's
//!     chunks, with a representative `file` + `language` borrowed from those
//!     chunks. `min(...)` makes the borrowed pick deterministic across runs.
//!   * **Edges** = every typed edge whose **source** entity is mentioned in
//!     one of the workspace's chunks. This mirrors the scoping the live
//!     ingest test asserts over (`Chunk{workspace}<-[:MENTIONED_IN]-(e)-[r]->(t)`).
//!     Edge *targets* may be external (an imported stdlib module, an
//!     inherited base that is never itself mentioned in a chunk); the
//!     [`super::projection`] layer synthesises minimal nodes for them so
//!     every edge endpoint resolves.
//!
//! Because edges are global-by-name, source-scoping is exact in the common
//! single-project setup and, in a shared graph, may also surface an edge
//! another workspace wrote from a same-named source — the inherent trade-off
//! of the shared entity space, documented here so a later slice can tighten
//! it if needed.

/// Workspace-scoped entity nodes. One row per distinct entity name, with a
/// deterministic representative `file` + `language` from the workspace's
/// chunks (`min` over the candidates). `language`/`file` can be empty.
pub const NODES_FOR_WORKSPACE: &str = "
MATCH (c:Chunk {workspace: $ws})<-[:MENTIONED_IN]-(e:Entity)
RETURN e.name AS name, min(c.path) AS file, min(c.language) AS language
";

/// Workspace-scoped typed edges. One row per distinct `(src, rel, dst)`
/// whose `src` entity is mentioned in the workspace. Covers the full
/// Entity→Entity relation vocabulary ([`super::schema`]); ingest writes
/// `CALLS`/`IMPORTS`/`INHERITS` today, the other two are matched for
/// forward-compatibility.
pub const EDGES_FOR_WORKSPACE: &str = "
MATCH (c:Chunk {workspace: $ws})<-[:MENTIONED_IN]-(e:Entity)
      -[r:CALLS|IMPORTS|INHERITS|CONFIGURES|EXPOSES]->(t:Entity)
RETURN DISTINCT e.name AS src, t.name AS dst, type(r) AS rel
";

/// One decoded node row from [`NODES_FOR_WORKSPACE`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeRow {
    /// The entity name — its stable key in the graph (also the v1 node id).
    pub name: String,
    /// A representative source file the entity is mentioned in (may be empty).
    pub file: String,
    /// The file's language tag (may be empty).
    pub language: String,
}

/// One decoded edge row from [`EDGES_FOR_WORKSPACE`]. `rel` is the raw
/// `type(r)` string (e.g. `"CALLS"`); [`super::projection`] parses it into a
/// [`super::projection::RelType`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EdgeRow {
    pub src: String,
    pub dst: String,
    pub rel: String,
}

/// The raw rows the projection layer turns into a `WorkspaceGraph`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GraphRows {
    pub nodes: Vec<NodeRow>,
    pub edges: Vec<EdgeRow>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_query_is_workspace_scoped_via_mentioned_in() {
        for needle in ["Chunk {workspace: $ws}", "MENTIONED_IN", "Entity", "e.name AS name"] {
            assert!(
                NODES_FOR_WORKSPACE.contains(needle),
                "node query missing {needle}"
            );
        }
    }

    #[test]
    fn edge_query_covers_full_relation_vocabulary_and_scopes_by_source() {
        for needle in [
            "Chunk {workspace: $ws}",
            "MENTIONED_IN",
            "type(r) AS rel",
            "DISTINCT",
        ] {
            assert!(
                EDGES_FOR_WORKSPACE.contains(needle),
                "edge query missing {needle}"
            );
        }
        for rel in ["CALLS", "IMPORTS", "INHERITS", "CONFIGURES", "EXPOSES"] {
            assert!(
                EDGES_FOR_WORKSPACE.contains(rel),
                "edge query omits {rel} from the rel-type filter"
            );
        }
    }

    #[test]
    fn neither_read_query_mutates() {
        for q in [NODES_FOR_WORKSPACE, EDGES_FOR_WORKSPACE] {
            for forbidden in ["MERGE", "CREATE", "DELETE", "SET ", "REMOVE"] {
                assert!(
                    !q.contains(forbidden),
                    "read query unexpectedly contains {forbidden}: {q}"
                );
            }
        }
    }
}
