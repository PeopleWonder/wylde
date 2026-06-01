---
title: Extending the memgraph client
audience: contributors changing graph schema, Cypher templates, or the Bolt transport
authored: 2026-05-27
status: living reference
---

# Memgraph

## Executive summary

Memgraph is the **graph database** that lets Wylde reason about
relationships. Long-term memory and the vector store can answer "what
notes are similar to this question" — that's a flat similarity
problem. Memgraph answers "what calls this function," "what
configures this service," "which entities are mentioned together a
lot." Those are graph-shaped questions, and a graph database is the
right tool. Wylde uses Neo4j (the open-source community edition)
running as a bundled JVM process underneath; the Python supervisor
just makes sure the JVM stays alive.

The Rust harness talks to Neo4j directly over the **Bolt protocol**
using the `neo4rs` crate. This is new — until late May 2026 the
harness went through a Python intermediary that translated each
request to Cypher and back; the direct-Bolt path cuts out the
intermediary, which is one fewer process, one fewer wire format, and
materially lower latency. The translation logic (which Cypher to send
for which verb) lives in `cypher.rs` as plain string constants ported
verbatim from the Python service.

The schema is small and deliberate: two node kinds (`Entity`, `Chunk`),
six relation kinds (`CALLS`, `IMPORTS`, `INHERITS`, `CONFIGURES`,
`EXPOSES`, `MENTIONED_IN`). Adding a relation type or a node kind is
straightforward but touches many files; the doc walks through it.
The traversal API (multi-hop, bucket-filtered) is exposed via the
`MemgraphTraversal` trait so both the Bolt path and the legacy pipe
path implement the same surface.

## How it works

### Files

```
rust/crates/wylde-harness/src/memory/memgraph/
├── mod.rs              — re-exports
├── schema.rs           — node labels, relation types, bucket constants
├── cypher.rs           — Cypher templates (port of Python's _driver.py)
├── transport.rs        — MemgraphTraversal trait + TraversalImpl dispatcher
├── client.rs           — pipe transport (legacy, rollback only)
├── bolt.rs             — direct Bolt transport via neo4rs (canonical)
├── graph_retrieval.rs  — expand_by_graph() hybrid expander
└── actions.rs          — pipe-action handlers for meta.graph_query
```

### Schema (`schema.rs`)

**Node labels:**

* `Entity` — code symbols (functions, classes, modules, configs).
* `Chunk` — text fragments from indexed files.

**Relation types** (all Entity→Entity except where noted):

* `CALLS` — direct call site references.
* `IMPORTS` — module / package imports.
* `INHERITS` — class / trait inheritance.
* `CONFIGURES` — configuration applied to a target entity.
* `EXPOSES` — outward-facing surface (route, action, public symbol).
* `MENTIONED_IN` (Entity→Chunk) — entity is mentioned in a chunk.

**Traversal buckets:**

* `BUCKET_CALLS_IMPORTS` ("calls_imports") — typically 1-hop, fast.
* `BUCKET_CONFIGURES_EXPOSES` ("configures_exposes") — typically 2-hop,
  broader.

`relation_type_is_valid(rel) -> bool` is the client-side validation —
five Entity→Entity types are accepted; `MENTIONED_IN` is intentionally
excluded because it's edge-of-system structural, not a user-facing
relation type.

### Cypher templates (`cypher.rs`)

All graph queries are static Cypher strings (or simple format-strings
where Cypher syntax forbids parameterisation — multihop depth, for
instance, can't use `$depth` in the quantifier position). The set:

* `SCHEMA_STATEMENTS: &[&str]` — `CREATE INDEX IF NOT EXISTS` for
  entity name, chunk id, community_cid. Idempotent; called by
  `ensure_schema`.
* `UPSERT_ENTITIES: &str` — merge chunks + entities + `MENTIONED_IN`
  edges; clears stale edges per chunk to avoid re-emit cruft.
* `DELETE_PATH: &str` — `DETACH DELETE` every chunk for a path.
* `DELETE_WORKSPACE_CHUNKS` + `DELETE_ORPHAN_ENTITIES` — two-step
  workspace delete (chunks first, then orphan entity prune).
* `multihop_expand(depth) -> String` — co-mentioned entities within
  `depth * 2` Entity-Chunk-Entity hops.
* `multihop_chunks: &str` — chunks ranked by entity-set overlap.
* `traverse_bucket(rel_types, depth, with_workspace) -> String` —
  per-bucket cypher with interpolated edge quantifier + optional
  workspace filter.

The Python service has the same set in `Core/Memgraph/graph_service/`.
**Python is the source of truth until both sides have parity tests** —
if a template diverges, port the Python version, not the other way
around.

### Transports (`client.rs`, `bolt.rs`, `transport.rs`)

Two implementations live side by side:

* **`Client`** (`client.rs`) — talks msgpack-over-named-pipe to the
  Python `wylde-memgraph` service, which translates to Cypher.
  Rollback path; selected when `WYLDE_HARNESS_MEMORY_IMPL=python`.
* **`BoltClient`** (`bolt.rs`) — direct Bolt to Neo4j via `neo4rs`.
  Canonical; selected when `WYLDE_HARNESS_MEMORY_IMPL=rust` (the
  default as of 2026-05-26).

Both implement the **`MemgraphTraversal`** trait
(`transport.rs:48-59`):

```rust
pub trait MemgraphTraversal {
    fn traverse(&self, req: TraverseRequest)
        -> impl Future<Output = Reply>;
    fn multihop(&self, entities: Vec<String>, expand_hops: u32, limit: u32)
        -> impl Future<Output = Reply>;
}
```

The dispatcher `current_traversal_impl() -> TraversalImpl` reads the
env var and returns the right enum variant; consumers
(`graph_retrieval`, `actions`) pattern-match on `TraversalImpl::Pipe`
vs `TraversalImpl::Bolt`. Static dispatch — no `Box<dyn Trait>`.

### The Bolt client (`bolt.rs`)

`BoltClient` wraps a `neo4rs::Graph` (the connection pool) plus error
recovery (a `DRIVER_ERROR_TTL` backoff for transient failures). Public
methods mirror the pipe client one-for-one:

* `health()` — `RETURN 1` ping.
* `ensure_schema()` — run `SCHEMA_STATEMENTS`.
* `upsert(batch)` — run `UPSERT_ENTITIES` with `$batch`.
* `relate(src, dst, rel_type)`, `unrelate(src, dst, rel_type)` —
  individual edge writes.
* `traverse(req)`, `multihop(...)` — read paths.
* `delete_path(path)`, `delete_workspace(ws)`, `delete_orphan_entities()`
  — destructive cleanup.
* `stats()` — node + edge counts.

The Bolt URL is configurable via `MEMGRAPH_BOLT_URL` (default
`bolt://localhost:7687`); credentials via `MEMGRAPH_USER` /
`MEMGRAPH_PASSWORD` (default `neo4j` / `wylde`).

### Hybrid expansion (`graph_retrieval.rs`)

`expand_by_graph(client, seed_entities, hops, limit) -> Vec<ChunkHit>`
is the hybrid-retrieval entry point. Given a list of seed entities
(usually parsed from a user query by NER or extracted from initial
vector hits), it:

1. Calls `multihop` to expand the seed set within `hops`
   Entity-Chunk-Entity steps.
2. Pulls chunks ranked by overlap with the expanded entity set.
3. Returns chunks with graph-derived similarity scores.

This is half of the RAG hybrid path — see [rag.md](./rag.md) for the
merge step that fuses these with vector hits.

### The action surface

`actions.rs` registers `meta.graph_query` on the pipe. Payload:
`{query_vector?, entities?, hops?, limit?}`. With a query_vector, the
hybrid path runs (vector + graph fusion); without, the entity-only
path runs. Returns ranked chunks tagged with `source: "memgraph"` or
`source: "memgraph+vector"`.

## How to extend

### Add a new relation type

E.g. `REPLIES_TO` for chat-thread structure:

1. Add to `schema.rs`:
   ```rust
   pub const REL_REPLIES_TO: &str = "REPLIES_TO";
   ```
2. Decide if it's user-settable via `/relate` — if yes, add to
   `relation_type_is_valid`:
   ```rust
   pub fn relation_type_is_valid(rel: &str) -> bool {
       matches!(rel,
           REL_CALLS | REL_IMPORTS | REL_INHERITS
           | REL_CONFIGURES | REL_EXPOSES | REL_REPLIES_TO)
   }
   ```
3. If you want a traversal bucket for it, add a `BUCKET_REPLIES`
   constant and extend `traverse_bucket`'s match.
4. Coordinate with Python — the server-side `/relate` route has its
   own validation list; if the lists drift, the harness emits a
   relation the server refuses.

### Add a new Cypher template

E.g. a "most-mentioned entities in the last N days" query:

1. Add to `cypher.rs`:
   ```rust
   pub fn top_entities_in_window(days: u32) -> String {
       format!("
           MATCH (e:Entity)-[r:MENTIONED_IN]->(c:Chunk)
           WHERE c.created_at > timestamp() - {ms}
           WITH e, count(r) AS mentions
           ORDER BY mentions DESC
           LIMIT 20
           RETURN e.name AS name, mentions
       ", ms = days * 86_400_000)
   }
   ```
2. Add a method to `BoltClient` that runs it.
3. Add a matching method to `Client` (pipe) for the rollback path.
4. (Optional) Expose as an LLM tool in `tooling/tools/rag.rs` or
   `meta.rs`.

### Add a new node kind

E.g. `Community` for entity clusters. This is a bigger change:

1. Add `NODE_COMMUNITY: &str = "Community"` to `schema.rs`.
2. Add `CREATE INDEX community_cid IF NOT EXISTS FOR (n:Community) ON
   (n.community_id)` to `SCHEMA_STATEMENTS` (already there, in fact —
   the convention is pre-declared).
3. Add write operations (`upsert_community`, `link_entity_to_community`)
   to both `Client` and `BoltClient`.
4. Add read operations (`get_community_for_entity`,
   `list_entities_in_community`).
5. Decide whether `graph_retrieval::expand_by_graph` should follow
   community membership.
6. Add corresponding Cypher templates to `cypher.rs`.

### Swap the Bolt driver

`neo4rs` is one of several Rust Neo4j drivers. If you wanted to swap
to `bolt-client` or roll your own:

* `BoltClient` is the only consumer of `neo4rs::Graph`.
* Wrap the new driver in a struct with the same method signatures.
* The `MemgraphTraversal` trait impl stays unchanged; only the body
  swaps.

### Add a new transport (e.g. gRPC to a remote graph)

Two clients today (pipe, Bolt). A third (remote gRPC, say):

1. Add `GrpcClient` in a new `grpc.rs`.
2. Implement `MemgraphTraversal` for it.
3. Add `TraversalImpl::Grpc(GrpcClient)` variant to the dispatcher.
4. Add `WYLDE_HARNESS_MEMORY_IMPL=grpc` recognition in
   `current_traversal_impl`.

## Gotchas

* **Cypher's relationship-quantifier doesn't accept `$` parameters.**
  This is why `multihop_expand(depth) -> String` interpolates the
  depth into the string rather than passing it as a parameter. The
  format-string approach is safe (depth is an integer from typed code,
  not user input), but the pattern is unusual and trips up reviewers.
* **The Python server validates `rel_type` server-side.** If you add
  a relation to `schema.rs::relation_type_is_valid` without adding it
  to the Python route's allowlist, the harness will emit edges the
  pipe path refuses. Update both sides.
* **`MENTIONED_IN` is excluded from `relation_type_is_valid` on
  purpose.** It's an internal edge written by `UPSERT_ENTITIES`, not a
  user-facing relation. Don't add it to `/relate`.
* **`DETACH DELETE` is destructive and not transactional across
  statements.** `delete_workspace` is a two-step process (chunks
  first, then orphan entities). If the second step fails, you've got
  chunks deleted but orphan entities still around. The next upsert
  will re-MERGE entities it needs, so the practical impact is small —
  but if you add a delete operation, follow the same two-step pattern.
* **The Bolt connection pool can stale.** `BoltClient` has a
  `DRIVER_ERROR_TTL` retry, but if the Neo4j JVM restarts (e.g. when
  the Python supervisor decides to recycle it), in-flight queries
  fail until the pool reconnects. Defensive callers retry on
  `Transport(_)` errors.
* **Schema indexes are `IF NOT EXISTS` — adding an index doesn't drop
  the old one.** If you change `entity_name` from `(n.name)` to
  `(n.name, n.kind)`, the old index stays and the new one is created
  alongside; queries that hit just `n.name` still use the old one,
  inefficiently. Drop the old explicitly via `DROP INDEX entity_name`
  in a migration cypher.

## Cross-links

* [index.md](./index.md) — memory subsystem overview.
* [rag.md](./rag.md) — the hybrid retrieval path that fuses
  `expand_by_graph` results with vector hits.
* `Core/Memgraph/graph_service/_driver.py` — the Python source of
  truth for Cypher templates.
* `Core/Memgraph/run.py` — the Python supervisor for the bundled
  Neo4j JVM (stays Python; the harness only talks Bolt directly).
* `~/.claude/projects/.../memory/wylde_memgraph_direct_bolt.md` — the
  direct-Bolt architecture memo.
* `~/.claude/projects/.../memory/wylde_phase7b_memgraph_direct_bolt_shipped.md`
  — the 2026-05-26 cutover.

---

*Two transports, six relations, two node kinds. The schema is small;
keep it that way. Every new node or relation is a months-long
coordination cost.*
