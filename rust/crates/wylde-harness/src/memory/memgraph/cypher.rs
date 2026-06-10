//! Cypher templates and per-verb query strings — ported verbatim from
//! `Core/Memgraph/graph_service/_driver.py` + `_routes_*.py`.
//!
//! The Python service translates each pipe verb to a Cypher statement
//! and ships it to Neo4j over Bolt. When the harness picks the
//! `WYLDE_HARNESS_MEMORY_IMPL=rust` strangler-fig branch, the Rust
//! [`super::bolt::BoltClient`] does the same translation **without** the
//! Flask round-trip — same Cypher, same Neo4j, fewer hops.
//!
//! Constants here are the single source of truth on the Rust side. The
//! Python module stays canonical until the cutover slice (later); if
//! the two ever diverge, defer to Python while the default impl is
//! still `python`.

/// `/ensure_schema` — idempotent index creation. Each statement is
/// `IF NOT EXISTS`, safe to re-run after every boot. Mirrors Python's
/// `_SCHEMA_CYPHER` list in `_driver.py`.
pub const SCHEMA_STATEMENTS: &[&str] = &[
    "CREATE INDEX entity_name   IF NOT EXISTS FOR (n:Entity)    ON (n.name)",
    "CREATE INDEX chunk_id      IF NOT EXISTS FOR (n:Chunk)     ON (n.id)",
    "CREATE INDEX community_cid IF NOT EXISTS FOR (n:Community) ON (n.community_id)",
];

/// `/upsert` — merge chunks + entities + MENTIONED_IN edges. Stale
/// MENTIONED_IN edges are cleared per-chunk so a re-emit doesn't
/// accumulate cruft. Mirrors Python's `_UPSERT_ENTITIES_CYPHER`.
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

/// `/delete_path` — DETACH DELETE every Chunk node for a path.
pub const DELETE_PATH: &str = "MATCH (c:Chunk {path: $path}) DETACH DELETE c";

/// `/delete_workspace` step 1 — DETACH DELETE every Chunk in a workspace.
pub const DELETE_WORKSPACE_CHUNKS: &str = "
MATCH (c:Chunk {workspace: $ws})
WITH count(c) AS n, collect(c) AS cs
UNWIND cs AS c DETACH DELETE c
RETURN n
";

/// `/delete_workspace` step 2 — prune Entity nodes whose only edges
/// were MENTIONED_IN edges into the now-deleted chunks. Mirrors
/// Python's orphan-pruning pass — the next `/upsert` will re-MERGE
/// any entity it needs.
pub const DELETE_ORPHAN_ENTITIES: &str = "
MATCH (e:Entity)
WHERE NOT (e)-[:MENTIONED_IN]->(:Chunk)
WITH count(e) AS n, collect(e) AS es
UNWIND es AS e DETACH DELETE e
RETURN n
";

/// `/multihop` step 1 — collect co-mentioned entities within
/// `expand_hops * 2` Entity-Chunk-Entity hops of the seeds. Mirrors
/// Python's `cypher_expand` in `_routes_traverse.multihop`. The hop
/// depth is interpolated, not parameterised, because Cypher does not
/// allow `$`-substitution in relationship-quantifier positions.
pub fn multihop_expand(depth: u32) -> String {
    format!(
        "
UNWIND $names AS seed_name
MATCH (seed:Entity {{name: seed_name}})
MATCH (seed)-[:MENTIONED_IN*1..{depth}]-(other:Entity)
WITH DISTINCT other.name AS name, count(*) AS w
ORDER BY w DESC
LIMIT 200
RETURN collect(name) AS names
"
    )
}

/// `/multihop` step 2 — chunks ranked by how many of the expanded
/// entity set they touch. Limit + per-name list are parameterised.
pub const MULTIHOP_CHUNKS: &str = "
UNWIND $names AS name
MATCH (e:Entity {name: name})-[:MENTIONED_IN]->(c:Chunk)
WITH c, count(DISTINCT e) AS hits
RETURN c.id AS id, c.path AS path, c.symbol AS symbol,
       c.language AS language, hits
ORDER BY hits DESC
LIMIT $limit
";

/// `/traverse` per-bucket Cypher. Built per call so the typed-edge
/// quantifier (`*0..depth`) and workspace filter can be interpolated;
/// the Python `_build_cypher` does the same. `rel_types` is the
/// `CALLS|IMPORTS|INHERITS` (or `CONFIGURES|EXPOSES`) alternation.
pub fn traverse_bucket(rel_types: &str, depth: u32, with_workspace: bool) -> String {
    let ws_filter = if with_workspace {
        " AND c.workspace = $ws "
    } else {
        ""
    };
    format!(
        "
UNWIND $names AS name
MATCH  (seed:Entity {{name: name}})
MATCH  p = (seed)-[:{rel_types}*0..{depth}]-(e:Entity)-[:MENTIONED_IN]->(c:Chunk)
WHERE  1=1{ws_filter}
WITH   c, seed, min(length(p) - 1) AS typed_depth
WITH   c, count(DISTINCT seed) AS seeds_touching, min(typed_depth) AS best_depth
RETURN c.id       AS id,
       c.path     AS path,
       c.symbol   AS symbol,
       c.language AS language,
       seeds_touching,
       best_depth
"
    )
}

/// `/traverse` bucket relation alternation — CALLS|IMPORTS|INHERITS.
/// Typically 1-hop in the Python server because these edges are
/// point-to-point and high-precision.
pub const REL_ALT_CALLS: &str = "CALLS|IMPORTS|INHERITS";

/// `/traverse` bucket relation alternation — CONFIGURES|EXPOSES.
/// Typically 2-hop because the route can reach config sections that
/// govern downstream services.
pub const REL_ALT_CFG: &str = "CONFIGURES|EXPOSES";

/// `/relate` — typed Entity→Entity edges. Built per `rel_type` because
/// Cypher does not allow `$`-substitution in relationship-type
/// positions; the caller MUST validate `rel_type` against
/// [`super::schema::relation_type_is_valid`] before interpolating.
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

/// `/unrelate` — drop typed Entity→Entity edges. Same interpolation
/// constraint as `/relate`.
pub fn unrelate_typed(rel_type: &str) -> String {
    format!(
        "
UNWIND $pairs AS row
MATCH (a:Entity {{name: row.source}})-[r:{rel_type}]->(b:Entity {{name: row.target}})
DELETE r
"
    )
}

/// `/upsert_edge` — MERGE-style weighted edge upsert. Used by the RAG
/// feedback loop — a successful cited retrieval strengthens the
/// `source -[label]-> target` edge, a miss leaves a low-weight trail.
/// `label` is interpolated for the same Cypher-syntax reason as
/// `relate_typed`; the caller validates it.
pub fn upsert_edge(label: &str) -> String {
    format!(
        "
MERGE (s {{name: $source}})
MERGE (t {{name: $target}})
MERGE (s)-[r:{label}]->(t)
ON CREATE SET r.weight = $weight_delta
ON MATCH  SET r.weight = coalesce(r.weight, 0) + $weight_delta
"
    )
}

/// `/stats` — five counts the Python service returns. We issue them as
/// separate queries to match the Python shape exactly.
pub mod stats {
    pub const COUNT_ENTITIES: &str = "MATCH (:Entity) RETURN count(*) AS n";
    pub const COUNT_CHUNKS: &str = "MATCH (:Chunk) RETURN count(*) AS n";
    pub const COUNT_MENTIONS: &str = "MATCH ()-[r:MENTIONED_IN]->() RETURN count(r) AS n";
    pub const COUNT_COMMUNITIES: &str = "MATCH (:Community) RETURN count(*) AS n";
    pub const COUNT_TYPED_RELATIONSHIPS: &str = "MATCH ()-[r]->() WHERE type(r) IN \
         ['CALLS','IMPORTS','INHERITS','CONFIGURES','EXPOSES'] RETURN count(r) AS n";
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_statements_are_idempotent() {
        for stmt in SCHEMA_STATEMENTS {
            assert!(
                stmt.contains("IF NOT EXISTS"),
                "schema stmt must be idempotent: {stmt}"
            );
        }
    }

    #[test]
    fn schema_statements_cover_three_indexes() {
        assert_eq!(SCHEMA_STATEMENTS.len(), 3);
        let joined = SCHEMA_STATEMENTS.join(" ");
        assert!(joined.contains("entity_name"));
        assert!(joined.contains("chunk_id"));
        assert!(joined.contains("community_cid"));
    }

    #[test]
    fn upsert_entities_mentions_required_fields() {
        // Touchpoints the upsert handler binds via $batch — if any of
        // these go missing the route silently no-ops on that property.
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
    fn multihop_expand_interpolates_depth() {
        let q = multihop_expand(4);
        assert!(q.contains("MENTIONED_IN*1..4"));
        assert!(q.contains("$names"));
    }

    #[test]
    fn traverse_bucket_omits_workspace_filter_when_unset() {
        let q = traverse_bucket(REL_ALT_CALLS, 1, false);
        assert!(q.contains("CALLS|IMPORTS|INHERITS"));
        assert!(q.contains("*0..1"));
        assert!(!q.contains("c.workspace"));
        assert!(!q.contains("$ws"));
    }

    #[test]
    fn traverse_bucket_includes_workspace_filter_when_set() {
        let q = traverse_bucket(REL_ALT_CFG, 2, true);
        assert!(q.contains("CONFIGURES|EXPOSES"));
        assert!(q.contains("*0..2"));
        assert!(q.contains("c.workspace = $ws"));
    }

    #[test]
    fn relate_and_unrelate_interpolate_rel_type() {
        let r = relate_typed("CALLS");
        assert!(r.contains("[:CALLS]"));
        assert!(r.contains("$pairs"));
        let u = unrelate_typed("EXPOSES");
        assert!(u.contains("[r:EXPOSES]"));
        assert!(u.contains("DELETE r"));
    }

    #[test]
    fn upsert_edge_uses_weight_delta_with_coalesce() {
        let q = upsert_edge("MENTIONS");
        assert!(q.contains("[r:MENTIONS]"));
        assert!(q.contains("$weight_delta"));
        assert!(q.contains("coalesce(r.weight, 0)"));
    }

    #[test]
    fn stats_queries_return_count_alias() {
        for q in [
            stats::COUNT_ENTITIES,
            stats::COUNT_CHUNKS,
            stats::COUNT_MENTIONS,
            stats::COUNT_COMMUNITIES,
            stats::COUNT_TYPED_RELATIONSHIPS,
        ] {
            assert!(q.contains("AS n"), "stats query must alias count as n: {q}");
        }
    }
}
