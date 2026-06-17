//! Cypher templates for the workspace graph-ingest writes.
//!
//! A narrow relocation of the harness `memory::memgraph::cypher` (Slice 0b):
//! only the statements the workspace ingest + cleanup paths use (`upsert`,
//! `relate`, `delete_workspace`). The strings are byte-identical to the
//! harness copies so the two services write the same shapes to the one Neo4j.

/// `upsert` — merge chunks + entities + MENTIONED_IN edges. Stale
/// MENTIONED_IN edges are cleared per-chunk so a re-emit doesn't accumulate
/// cruft.
pub const UPSERT_ENTITIES: &str = "
UNWIND $batch AS row
MERGE (c:Chunk {id: row.id})
SET   c.path = row.path, c.symbol = row.symbol, c.language = row.language,
      c.workspace = row.workspace
WITH  c, row
OPTIONAL MATCH (c)<-[e:MENTIONED_IN]-(:Entity)
DELETE e
WITH  c, row
UNWIND row.entities AS ent_name
MERGE (e:Entity {name: ent_name})
MERGE (e)-[:MENTIONED_IN]->(c)
";

/// `delete_workspace` step 1 — DETACH DELETE every Chunk in a workspace.
pub const DELETE_WORKSPACE_CHUNKS: &str = "
MATCH (c:Chunk {workspace: $ws})
WITH count(c) AS n, collect(c) AS cs
UNWIND cs AS c DETACH DELETE c
RETURN n
";

/// `delete_file_nodes` step 1 (Slice I — file watcher) — DETACH DELETE every
/// Chunk for one workspace whose `path` is exactly `$path` OR sits under it
/// (`$prefix` = `$path` + the platform separator). The exact match covers a
/// single deleted/renamed file; the prefix match covers a deleted directory's
/// whole subtree in one statement. Scoped by `workspace` so a shared file path
/// across workspaces only drops the active one's chunks. Shares the
/// orphan-entity prune ([`DELETE_ORPHAN_ENTITIES`]) with `delete_workspace`.
pub const DELETE_FILE_CHUNKS: &str = "
MATCH (c:Chunk {workspace: $ws})
WHERE c.path = $path OR c.path STARTS WITH $prefix
WITH count(c) AS n, collect(c) AS cs
UNWIND cs AS c DETACH DELETE c
RETURN n
";

/// `delete_workspace` step 2 — prune Entity nodes whose only edges were
/// MENTIONED_IN edges into the now-deleted chunks.
pub const DELETE_ORPHAN_ENTITIES: &str = "
MATCH (e:Entity)
WHERE NOT (e)-[:MENTIONED_IN]->(:Chunk)
WITH count(e) AS n, collect(e) AS es
UNWIND es AS e DETACH DELETE e
RETURN n
";

/// Concept projection (TBS concept-system, thesis §2.2) — additive sync of the
/// authoritative JSON concept store into the graph so the panel can render
/// concept nodes. MERGE each `Concept` (keyed by `workspace`+`id`), refresh its
/// label/description, clear stale `MEMBER`/`CHILD_OF` edges for that concept,
/// then re-create them from the row. Member targets are matched by Entity name
/// (the graph's entity key); a member with no Entity node is simply skipped
/// (OPTIONAL MATCH). Fail-soft: the upsert is best-effort enrichment.
pub const UPSERT_CONCEPTS: &str = "
UNWIND $batch AS row
MERGE (k:Concept {workspace: row.workspace, id: row.id})
SET   k.label = row.label, k.description = row.description, k.source = row.source
WITH  k, row
OPTIONAL MATCH (k)-[m:MEMBER]->()
DELETE m
WITH  k, row
OPTIONAL MATCH (k)-[c:CHILD_OF]->()
DELETE c
WITH  k, row
CALL {
  WITH k, row
  UNWIND row.members AS ent_name
  MATCH (e:Entity {name: ent_name})
  MERGE (k)-[:MEMBER]->(e)
}
WITH k, row
UNWIND (CASE WHEN size(row.parents) = 0 THEN [null] ELSE row.parents END) AS parent_id
WITH k, row, parent_id WHERE parent_id IS NOT NULL
MATCH (p:Concept {workspace: row.workspace, id: parent_id})
MERGE (k)-[:CHILD_OF]->(p)
";

/// Concept projection cleanup — DETACH DELETE every `Concept` in a workspace
/// (the build pass replaces the whole set; clear before re-projecting).
pub const DELETE_WORKSPACE_CONCEPTS: &str = "
MATCH (k:Concept {workspace: $ws})
WITH count(k) AS n, collect(k) AS ks
UNWIND ks AS k DETACH DELETE k
RETURN n
";

/// `relate` — typed Entity→Entity edges. Built per `rel_type` because Cypher
/// disallows `$`-substitution in relationship-type positions; the caller MUST
/// validate `rel_type` against [`super::schema::relation_type_is_valid`].
pub fn relate_typed(rel_type: &str) -> String {
    format!(
        "
UNWIND $pairs AS row
MERGE (a:Entity {{name: row.source}})
MERGE (b:Entity {{name: row.target}})
MERGE (a)-[:{rel_type}]->(b)
"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_entities_mentions_required_fields() {
        for needle in [
            "$batch",
            "Chunk",
            "Entity",
            "MENTIONED_IN",
            "row.id",
            "row.workspace",
        ] {
            assert!(
                UPSERT_ENTITIES.contains(needle),
                "UPSERT_ENTITIES missing {needle}"
            );
        }
    }

    #[test]
    fn relate_interpolates_rel_type() {
        let r = relate_typed("CALLS");
        assert!(r.contains("[:CALLS]"));
        assert!(r.contains("$pairs"));
    }

    #[test]
    fn upsert_concepts_mentions_required_shapes() {
        for needle in [
            "$batch",
            "Concept",
            "row.workspace",
            "row.id",
            "MEMBER",
            "CHILD_OF",
            "row.members",
            "row.parents",
        ] {
            assert!(
                UPSERT_CONCEPTS.contains(needle),
                "UPSERT_CONCEPTS missing {needle}"
            );
        }
    }

    #[test]
    fn delete_workspace_concepts_is_scoped() {
        assert!(DELETE_WORKSPACE_CONCEPTS.contains("Concept {workspace: $ws}"));
        assert!(DELETE_WORKSPACE_CONCEPTS.contains("DETACH DELETE"));
    }
}
