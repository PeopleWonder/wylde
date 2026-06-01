---
title: Extending long-term memory
audience: contributors changing how Wylde scores or stores cross-workspace memories
authored: 2026-05-27
status: living reference
---

# Long-term memory

## Executive summary

Long-term memory is what Wylde knows about you that should survive
across conversations. "the Wylde user lives in England." "the Wylde user prefers
function-first code." "The Wylde project has a single auth boundary
at the VPN." These are the curated, slow-changing facts that the
model needs reliably — not just when the vector search happens to
find them, but ranked highly enough to actually surface in the next
chat.

To make that work, every long-term memory has more than just text. It
has an **importance score** from 1 to 10 (set by the LLM when the
memory is saved, or estimated from the body if the LLM didn't tag it),
a **last-used timestamp** that bumps every time the memory is recalled
(recent recall = recent relevance), and an **embedding** (a numeric
fingerprint of the content used for similarity search). When the
system goes to find relevant memories, it doesn't just sort by
similarity — it multiplies similarity by an importance weight and an
exponential decay over time, so a hand-flagged identity fact from a
month ago beats a passing remark from yesterday.

This doc explains the scoring formula in plain terms, walks through
the data structures, and shows the extension surfaces: changing the
decay constant, changing the importance heuristic, or adding a
reflection/promotion cycle. Reflection itself still lives in Python
(`Core/harness/memory/reflection.py`); the Rust side has the storage,
the scoring, and the read/write API but not the periodic LLM-driven
re-evaluation pass.

## How it works

### Files

```
rust/crates/wylde-harness/src/memory/long_term/
├── mod.rs           — re-exports
├── records.rs       — disk layout (JSON + bincode vector mirror)
├── entries.rs       — CRUD: save/get/update/delete/history/search/touch/core_block
├── scoring.rs       — pure functions: combined_score, heuristic_importance, normalize_importance
└── test_support.rs  — TestEnv (TEST_ENV_LOCK acquirer, tmp data dir, cleanup-on-drop)
```

### Data layout

* **`<data_dir>/long_term.json`** — authoritative `{"memories": [...]}`.
  Each entry: id, body, importance (1..10), last_used_at (epoch seconds),
  session_id, source_path, tags, created_at.
* **`<data_dir>/long_term.vec.bin`** — bincode vector mirror. One record
  per memory id, with the embedding. Pure-Rust format owned by
  [`memory/vector/`](./vector-store.md). Rebuilt by reindex if it drifts
  from the JSON.

The JSON is the source of truth — the vector file is a mirror that can
be regenerated from the JSON + a re-embed. Don't trust the vector file
in isolation.

### The scoring formula

From `scoring.rs:23-36`:

```rust
score = similarity * (importance / 10.0) * exp(-age_days / decay_days)
```

Three multiplied terms:

* **similarity** — cosine similarity between the query vector and the
  memory's embedding. Range: 0.0 to 1.0 (vectors are pre-normalised).
* **importance / 10.0** — clamps the LLM's 1..10 score to a 0..1
  weight. A 10 contributes its full similarity; a 5 halves it; a 1
  cuts it to 10%.
* **exp(-age_days / decay_days)** — exponential decay. With the default
  `DEFAULT_DECAY_DAYS = 30.0`, a 30-day-old memory is worth `1/e` ≈
  37% of its fresh self. A 60-day-old memory is `1/e²` ≈ 13.5%.
  A `touch` call (after a successful recall) bumps `last_used_at` to
  now, so memories that keep getting used keep their decay reset.

### The importance heuristic

When the LLM saves a memory it should pass an importance score, but
sometimes it doesn't. `heuristic_importance(body, entity_count)`
(`scoring.rs:42-47`) fills in:

```rust
score = 3 + min(body_len / 100, 4) + clamp(entity_count, 0, 3)
// then clamp to 1..8
```

The 9..10 band is **reserved** for hand-flagged identity facts ("the Wylde user's
name is the Wylde user") or hard preferences. The heuristic never auto-promotes
above 8. The base of 3 means a totally empty body with zero entities
still gets a 3 — there's a non-zero prior that "the LLM thought it was
worth saving."

`normalize_importance(raw, body, entity_count)` (`scoring.rs:54-63`)
coerces the LLM's float to an int and clamps to 1..10. NaN or `None`
falls back to the heuristic.

### Public CRUD API (`entries.rs`)

* `save(id, body, importance, session_id, source_path, vector)` —
  insert or upsert. JSON + vector mirror both updated.
* `get(id)` — one memory.
* `update(id, updates)` — patch fields (e.g. bump importance).
* `delete(id)` — remove from JSON and vector.
* `history(id)` — returns the record (history is a single entry today;
  the name reflects intended-future revision tracking).
* `search(query_vector, limit)` — top-K cosine similarity. **Caller
  pre-embeds the query.** Returns hits sorted by raw similarity (not
  `combined_score` — that's a separate step in the consumer).
* `touch(id)` — bump `last_used_at` to now. Called after a successful
  recall.
* `core_block(id)` — mark importance 10. Protected from auto-pruning.

### Concurrency

A process-wide `Mutex` serialises JSON read-modify-write. Holds are
short (sync file IO). Matches the Python predecessor's `threading.RLock`.
Vector mirror writes are inside the same critical section so JSON and
vector stay consistent.

### Reflection cycle (Python, for now)

`Core/harness/memory/reflection.py` (~619 LOC) runs periodic passes:

* **Importance promotion** — if a memory was recalled N times in M
  days, bump importance.
* **Chain pruning** — if two memories are near-duplicates, merge or
  delete the lower-importance one.
* **Identity protection** — never reduce importance below 9 for
  core_block entries.

The Rust port is a future slice (Phase 7.B+). Until then, the Rust path
writes through to JSON and the Python reflection job sees the writes on
its next cycle. Don't try to add reflection passes to the Rust path
without coordinating — you'd end up running them twice.

## How to extend

### Change the decay constant

`scoring.rs:14` defines `DEFAULT_DECAY_DAYS = 30.0`. Lowering it makes
memories age out faster (better for fast-moving preferences); raising
it makes them stickier (better for stable identity facts). The function
takes `decay_days` as a parameter, so per-tier overrides are already
possible — just pass a different value to `combined_score`.

If you change the default, write a parity test against the Python
`combined_score` so the cutover doesn't surprise anyone. Coordinated
constant changes are a strangler-fig hazard — different defaults on
either side of the impl flag mean different surfacing behaviour.

### Add a new importance heuristic

`heuristic_importance` is a single function. You can:

* Replace it inline if the new heuristic supersedes the old.
* Add a sibling (e.g. `heuristic_importance_v2`) and feature-gate it.
* Take a `Heuristic` trait if you need pluggability — but the heuristic
  has been stable for months; introducing a trait is probably overkill.

Minimal example — boost importance for memories that mention the
current workspace:

```rust
// scoring.rs
pub fn heuristic_importance_with_workspace(
    body: &str,
    entity_count: usize,
    workspace_slug: Option<&str>,
) -> i32 {
    let base = heuristic_importance(body, entity_count);
    match workspace_slug {
        Some(slug) if body.contains(slug) => (base + 1).clamp(1, 8),
        _ => base,
    }
}
```

Call this from `entries.rs::save` where the existing heuristic is
called. Don't forget the parity gate.

### Add a new field to the record

The JSON shape lives in `records.rs`. Adding a field (e.g. `priority:
Option<u8>` for an above-importance manual override):

1. Add the field to the struct in `records.rs` with `#[serde(default)]`
   so existing records round-trip cleanly.
2. Extend `save` / `update` in `entries.rs` to accept it.
3. Decide whether `combined_score` should consume it — if yes, update
   the formula and write a test.
4. Update the Python side simultaneously so the strangler-fig flip
   doesn't drop the field on round-trips.

### Wire it as an LLM tool

The LLM-facing `memory.save` / `memory.search` / etc. tools live in
`tooling/tools/memory.rs`. If you've added a new write path, expose it
as a tool there — see
[../../extending-wylde-llm-tools.md](../../extending-wylde-llm-tools.md)
for the recipe.

## Gotchas

* **The LLM doesn't always set importance.** Most saves come in with
  `importance: None`. The heuristic is the fallback for the majority
  case — if you "fix" the heuristic by tightening it, you may push a
  lot of legitimate-but-passing memories below the recall threshold.
  Look at a representative sample of recent saves before changing
  thresholds.
* **`touch` is the recall hook, not `search`.** `search` finds
  candidates; the *consumer* (e.g. the turn loop, after the model has
  used a result) is what calls `touch`. If you add a new search path,
  make sure something calls `touch` for the hits the user actually
  acted on. Otherwise decay punishes them unfairly.
* **`core_block` is for identity, not "important things."** Anything
  marked importance 10 by core_block is exempt from the reflection
  pruning pass. Overusing it bloats long-term memory with unprunable
  detritus. Reserve it for `name`, `pronouns`, `birthdate`, `address`,
  `hard preferences I will never change` — that kind of thing.
* **Pre-embedding is the caller's job.** `search(query_vector, ...)`
  takes a vector, not a query string. The embedder lives in
  wylde-ollama; when the Rust embedder ports (Phase 7.D), the
  deferred `memory.long_term.search` pipe verb unblocks. Until then,
  Python is canonical for the embed-then-search composite.
* **`TEST_ENV_LOCK` is shared.** Long-term tests serialise with every
  other memory submodule's tests. Don't hold the lock through
  multi-second work; if you need a slow test, mark it `#[ignore]` and
  run it explicitly.
* **The reflection cycle can rewrite your importance score.** A
  freshly saved memory with importance 7 may be promoted to 9 by the
  Python reflection job overnight if it was recalled enough times. If
  you're testing importance behaviour, mock the reflection cycle or
  use a fresh data dir.

## Cross-links

* [index.md](./index.md) — memory subsystem overview.
* [vector-store.md](./vector-store.md) — the storage substrate.
* [rag.md](./rag.md) — the four-tier RAG store; uses similar but
  separate scoring.
* `Core/harness/memory/reflection.py` — the Python reflection cycle.
* `Core/harness/memory/long_term.py` — Python canonical implementation.
* `~/.claude/projects/.../memory/wylde_phase7b_long_term_shipped.md` —
  the Phase 7.B-1 ship report.

---

*Long-term memory is the "what Wylde knows about you" tier. The scoring
formula is the load-bearing piece — tune it carefully and always with
parity tests.*
