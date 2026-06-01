---
title: Extending the harness — memory subsystem overview
audience: contributors changing how Wylde remembers things
authored: 2026-05-27
status: living reference
---

# The memory subsystem

## Executive summary

"Memory" in Wylde means everything the system remembers between
conversations and within them. That's a few different things, not one
thing, and the right design for each is different. A passing remark
("I prefer dark mode") needs to survive a few turns but probably not
forever. A workspace ("this folder is my novel") needs to be searchable
and have its own indexes. A core fact about you ("the Wylde user lives in
England") needs to surface in any chat, ranked higher than random
search hits. A piece of code you cited last week is a different kind
of memory than a person's name.

Wylde models this as five distinct subsystems that work together: a
short-term tier (the messages from the current turn), workspaces
(folder-anchored RAG contexts with MRU eviction), a long-term tier
(curated cross-workspace facts with importance scoring), a vector
store (the underlying similarity-search engine for everything that
needs vector hits), a graph (Neo4j over Bolt, holding entities and
their relations), and RAG (a four-tier semantic store that combines
vector hits with graph expansion). Each has its own data layout, its
own scoring algorithm, its own extension points. Mixing them up is the
fastest way to ship the wrong feature.

This doc is the overview — what each subsystem is for, how they fit
together, and where to read deeper. The per-subsystem docs are
linked below; each has its own exec summary so you can scan the
collection before drilling in.

## How it works

### The five subsystems

```
┌──────────────────────────────────────────────────────────────────┐
│                       Memory in Wylde                            │
│                                                                  │
│   ┌─────────────────┐    ┌──────────────────────┐                │
│   │  Short-term     │    │  Long-term           │                │
│   │  (the current   │    │  (cross-workspace    │                │
│   │   turn's chat   │    │   curated facts;     │                │
│   │   history)      │    │   importance-scored) │                │
│   └─────────────────┘    └──────────┬───────────┘                │
│                                     │                            │
│   ┌──────────────────────┐          │                            │
│   │  Workspaces          │          │                            │
│   │  (folder-anchored;   │          │                            │
│   │   MRU eviction;      │          │                            │
│   │   per-workspace      │          │                            │
│   │   indexes + memory)  │          │                            │
│   └──────────┬───────────┘          │                            │
│              │                      │                            │
│              ▼                      ▼                            │
│   ┌─────────────────────────────────────────┐                    │
│   │           Vector store                  │                    │
│   │   (pure-Rust bincode; linear cosine;    │                    │
│   │    powers long-term + RAG)              │                    │
│   └─────────────────────────────────────────┘                    │
│                                                                  │
│   ┌──────────────────────┐    ┌──────────────────────┐           │
│   │  Memgraph (Neo4j)    │    │  RAG (four tiers:    │           │
│   │  entities + chunks   │◄───┤   core / episodic /  │           │
│   │  + relations         │    │   semantic /         │           │
│   │  (direct Bolt)       │    │   procedural)        │           │
│   └──────────────────────┘    └──────────────────────┘           │
│                                                                  │
└──────────────────────────────────────────────────────────────────┘
```

### What each subsystem does

* **Short-term** — the messages and tool calls from the current chat
  turn. Lives in memory only (turn-scoped). Not its own submodule; it's
  carried in `crate::state::TurnState`.

* **Workspaces** (`memory/workspaces/`) — folder-anchored RAG contexts.
  Each workspace is a directory on disk that the user has opted into;
  it gets a stable slug, an MRU position, a persona string, and (when
  the indexer ports from Python) a LanceDB-equivalent index folder
  and a workspace-memory folder for LLM-curated insights.
  → [workspaces.md](./workspaces.md).

* **Long-term** (`memory/long_term/`) — global cross-workspace curated
  facts. Each entry has a body, an importance score (1–10), a last-used
  timestamp, and an embedding. Surfaces in chat when its
  `combined_score` (similarity × importance × recency) wins against
  other candidates. → [long-term.md](./long-term.md).

* **Vector store** (`memory/vector/`) — the pure-Rust similarity-search
  engine that long-term and RAG both build on. Bincode-on-disk, linear
  cosine scan, dimension fixed at construction. Designed for Wylde's
  scale (low thousands of records); HNSW would only help at hundred-
  thousand-plus. → [vector-store.md](./vector-store.md).

* **Memgraph** (`memory/memgraph/`) — the Neo4j-backed graph store.
  Two node kinds (`Entity`, `Chunk`), six relation kinds (`CALLS`,
  `IMPORTS`, `INHERITS`, `CONFIGURES`, `EXPOSES`, `MENTIONED_IN`).
  Talks Bolt directly via the `neo4rs` crate as of the 2026-05-26
  cutover; the named-pipe transport is rollback only.
  → [memgraph.md](./memgraph.md).

* **RAG** (`memory/rag/`) — the four-tier semantic store (core,
  episodic, semantic, procedural) plus the retrieval pipeline.
  `search_with_graph` is the hybrid path that fuses vector hits with
  graph-expanded neighbours via the `merge_and_rank` algorithm.
  → [rag.md](./rag.md).

### How they connect

A typical "what does Wylde remember about X" request fans out across
several:

1. The turn loop pulls a few recent **short-term** messages for chat
   context (no per-subsystem fetch — they're already in `TurnState`).
2. For workspace-shaped requests, the relevant **workspace** is
   resolved by slug; the workspace's persona is included in the system
   prompt.
3. The model may emit a `rag.ask` tool call. The handler runs
   `search_with_graph` (in **RAG**), which:
   - Calls `search` (against the **vector store** for the requested
     tier).
   - Calls `expand_by_graph` (against **memgraph**) for entity-mentioned
     chunks.
   - Calls `merge_and_rank` to fuse the two result sets.
4. Separately, the model may emit a `memory_search` tool call (Phase 9
   deferred verb — needs the Rust embedder) for **long-term** hits.

The vector store is the shared substrate. Memgraph and RAG are the two
retrieval surfaces. Long-term is curated; RAG is corpus-derived;
workspaces are scope-defining. Short-term is conversational.

### Strangler-fig status

The whole `memory/` tree defaults to `WYLDE_HARNESS_MEMORY_IMPL=python`
except memgraph, which flipped to `rust` on 2026-05-26. The Rust
handlers are registered (reachable through the in-process tool catalog),
but the pipe verbs route to Python until parity tests gate the flip.
See `~/.claude/projects/.../memory/wylde_phase7b_memgraph_direct_bolt_shipped.md`
for the cutover precedent.

### Cross-cutting test helper

`memory/common.rs::TEST_ENV_LOCK` is a single `std::sync::Mutex<()>`
that every memory submodule's `test_support.rs` re-exports. Tests that
rebind `WYLDE_DATA_DIR` to a tempdir must hold this lock for setup +
teardown; otherwise two tests fighting over the env var corrupt each
other's data. The lock is intentionally process-wide, not per-submodule
— racing tmp dirs are far worse than serialised tests.

## How to extend

The four common extension shapes:

1. **Add a memory write path** (e.g. "save this code snippet as a
   procedural memory") → see [rag.md](./rag.md) for tier choice and the
   `save` API; the LLM-facing tool entry goes in
   `tooling/tools/memory.rs` or `rag.rs`.

2. **Add a new ranking heuristic** (e.g. "boost entries that mention
   the current workspace") → either modify
   [long-term.md](./long-term.md)'s `combined_score` formula (cross-cuts
   the whole long-term tier) or [rag.md](./rag.md)'s `merge_and_rank`
   constants (cross-cuts the hybrid path). Coordinate with Python
   parity before changing constants.

3. **Add a new graph relation** (e.g. "REPLIES_TO" for chat-thread
   structure) → see [memgraph.md](./memgraph.md) §schema. Add the
   constant, update the Cypher templates, and add a write path in
   `client.rs` / `bolt.rs`.

4. **Swap the vector-search algorithm** (e.g. plug in HNSW for a
   future scale) → see [vector-store.md](./vector-store.md). The current
   shape is monolithic; the swap means introducing a `Searcher` trait
   and a feature gate.

If you're not sure which subsystem to touch, ask: is this memory
**curated by the user** (long-term), **derived from a corpus**
(RAG), **about relations between things** (memgraph), or **bounded by
a folder** (workspaces)? The answer maps to the submodule.

## Gotchas

* **Don't write to two memory subsystems for the same fact.** A memory
  has one home. Writing the same thing to long-term and to a workspace
  + RAG tier means three reads to get it back, three writes to update
  it, and inconsistency when one of them drifts. Pick the right home;
  cross-link via id if you need the relationship.
* **Embeddings are not computed inside the memory subsystem.** Callers
  pre-embed and pass vectors. This is deliberate — the embedder is
  shared across long-term, RAG, and (eventually) workspaces, and lives
  with wylde-ollama. When the Rust embedder ports (Phase 7.D), the
  `memory.long_term.search` deferred verb unblocks.
* **Reflection cycles are Python today.** The importance-promotion
  pass, the chain-pruning pass, and the workspace-memory curation pass
  all live in `Core/harness/memory/reflection.py`. The Rust port is a
  future slice; until then, don't try to add a reflection step to the
  Rust path. Wire your write into the existing tier and let the
  Python reflection job find it.
* **`TEST_ENV_LOCK` is exclusive.** Tests across all memory submodules
  serialise on this one mutex. A test that holds it for a long time
  (e.g. a multi-second integration test) slows every other memory
  test. Tight test bodies are part of the contract.
* **The bincode vector format has a `version` field.** It's currently
  1. Bumping it (to add a per-record metadata blob, say) means writing
  a migration that reads version 1 and writes version 2. Don't skip
  the migration — production memory stores have months of accumulated
  data and reindexing from JSON is expensive.

## Per-subsystem deep dives

* [long-term.md](./long-term.md) — global curated memories, importance
  scoring, recency decay.
* [workspaces.md](./workspaces.md) — folder-anchored contexts, MRU,
  the indexer port.
* [vector-store.md](./vector-store.md) — pure-Rust bincode + cosine,
  HNSW swap path.
* [memgraph.md](./memgraph.md) — direct-Bolt client, Cypher templates,
  schema constants.
* [rag.md](./rag.md) — four tiers, hybrid retrieval, merge_and_rank.

## Cross-links

* [../extending-the-harness.md](../../extending-the-harness.md) — the
  harness submodule overview.
* [../../extending-wylde-llm-tools.md](../../extending-wylde-llm-tools.md)
  — the LLM-tool extension surface; many memory operations are
  exposed as tools.
* `docs/wylde-repo-organization.md` §3 — the harness reference,
  memory subsection.
* `Core/harness/memory/` — the Python tree; canonical for everything
  except memgraph.

---

*Memory subsystems are the largest and most layered part of the
harness. Get the right subsystem first; the rest is mechanical.*
