# Wylde Graph

Graph service: a thin supervisor around **vendored Neo4j Community** (Bolt/Cypher), hosting the **code graph** — entities and relations extracted from workspace code and chunk ingest. Despite the historical "Memgraph" name, the engine is Neo4j and the contents are code-structure data; there is no memory-to-memory schema today (memory records reach the graph only as entity edges from workspace saves). See `outputs/wylde-memory-fixes-plan.md` M9 for the deferred memory-edges design.

- Transport: `\\.\pipe\wylde-memgraph` (Neo4j Bolt port 7687)
- Run: `python run.py`
- Install: `pip install -r requirements.txt`

## Rust Bolt path (`WYLDE_HARNESS_MEMORY_IMPL=rust`)

The harness talks to Neo4j **directly over Bolt** via
`wylde-harness/src/memory/memgraph/bolt.rs` (`neo4rs` →
`bolt://127.0.0.1:7687`), skipping the pipe/Flask hop. Auth is disabled
(`GRAPH_USER`/`GRAPH_PASSWORD` default empty), matching the vendored
Neo4j's `conf/neo4j.conf` (`dbms.security.auth_enabled=false`,
`server.bolt.listen_address=127.0.0.1:7687`). Env knobs: `GRAPH_BOLT_URL`,
`GRAPH_USER`, `GRAPH_PASSWORD`, `WYLDE_BOLT_CONNECT_TIMEOUT_SECS`.

## Live-verified (2026-07-12)

The Bolt layer is now exercised end-to-end against the bundled Neo4j by
`wylde-harness/tests/memgraph_live.rs` (3 `#[ignore]`d tests). Proven
against a real graph — write, then read back: `upsert` lands
Chunk+Entity nodes, `MENTIONED_IN` edges and the typed `CALLS`/`IMPORTS`
edges; `traverse`/`multihop` reach those chunks from a seed entity;
`relate`/`unrelate` add/remove typed edges (reflected in
`stats.typed_relationships`); `upsert_edge` (the RAG-feedback weighted
edge) writes and re-applies idempotently; the real `memory.workspace.save`
handler's best-effort entity→graph write lands a `MENTIONED_IN` edge; and
`delete_workspace` drops a workspace's chunks **and** prunes the entities
left orphaned (no unbounded Entity leak). No bug was found in the Bolt
layer — the writes that had only ever been fire-and-forget are confirmed
to actually land and be queryable. See `docs/dev_setup.md` for how to
bring up the dev DB (needs **JDK 21**) and run the ignored tests.
