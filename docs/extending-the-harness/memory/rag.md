---
title: Extending RAG
audience: contributors adding retrievers, tiers, hybrid paths, or ranking heuristics
authored: 2026-05-27
status: living reference
---

# RAG

## Executive summary

RAG stands for "Retrieval-Augmented Generation." The idea is simple:
when the user asks the model a question, don't just rely on what the
model knows — first go retrieve the most relevant pieces of *your*
information (your notes, your code, your past conversations) and put
them in front of the model alongside the question. That way the model
answers with your context, not its training data.

Wylde's RAG store is divided into **four tiers** by the kind of memory
each holds: **core** (mission-critical facts, highest priority),
**episodic** (time-bound session/event memories), **semantic** (timeless
long-form knowledge), and **procedural** (how-to / skill memories). The
tiers are physically separate — each gets its own JSON authoritative
file and its own bincode vector mirror — so a query can be scoped to
just one tier (or all four) and the relevance scoring stays appropriate
to the kind of memory you're after.

The interesting bit is the **hybrid retrieval** path. Pure vector
search ("find chunks similar to this query") is good at the obvious
matches but bad at the lateral leaps ("you didn't ask about X but X
is mentioned in the same chunks as your topic, so it's relevant").
Pure graph search ("find chunks mentioning entities related to the
entities in your query") is good at the lateral leaps but bad at
literal similarity. `search_with_graph` runs both, then fuses the
results via a small ranking algorithm called `merge_and_rank` —
constants tuned so that "we found this two different ways" beats
"we found this strongly one way." This is the algorithm contributors
most often want to tweak; the doc covers it in detail.

## How it works

### Files

```
rust/crates/wylde-harness/src/memory/rag/
├── mod.rs           — re-exports + test_support gate
├── tiers.rs         — TIER_CORE / EPISODIC / SEMANTIC / PROCEDURAL constants
├── store.rs         — TieredStore (JSON + bincode mirror per tier)
├── search.rs        — search / search_logged / search_with_graph
├── merge.rs         — merge_and_rank (the fusion algorithm)
├── miss_log.rs      — telemetry: misses.jsonl + feedback.jsonl + chunk_usage.json
├── prune.rs         — filtered destructive cleanup
├── feedback.rs      — CITED_IN + RETRIEVAL_MISS edge writeback
├── ingest.rs        — N8N webhook composer (transport deferred)
├── actions.rs       — eight rag.* model-callable handlers
└── test_support.rs  — TestEnv (TEST_ENV_LOCK + tmp data dir)
```

### The four tiers (`tiers.rs`)

```rust
pub const TIER_CORE: &str       = "core";
pub const TIER_EPISODIC: &str   = "episodic";
pub const TIER_SEMANTIC: &str   = "semantic";
pub const TIER_PROCEDURAL: &str = "procedural";

pub const ALL_TIERS: &[&str] = &[TIER_CORE, TIER_EPISODIC, TIER_SEMANTIC, TIER_PROCEDURAL];
```

Order in `ALL_TIERS` is canonical for iteration. `is_known_tier(s) ->
bool` is the validator; unknown tiers raise `SearchError::UnknownTier`.

When in doubt about which tier:

* **Core** — "this is true and almost always relevant." Identity,
  mission-critical configs, top-level rules.
* **Episodic** — "this happened, here's when." Chat logs, decision
  points with timestamps, "we tried X on Tuesday."
* **Semantic** — "this is a fact about how things work." Documentation,
  explanations, design rationale.
* **Procedural** — "this is how you do X." Recipes, step-by-step
  guides, "run `cargo test -p wylde-harness`."

### `TieredStore` (`store.rs`)

The store layout mirrors long-term in shape: each tier has its own
authoritative JSON file and its own bincode vector mirror.

* `<data_dir>/rag_tiers.json` — `{"core": [...], "episodic": [...], ...}`.
* `<data_dir>/rag_tiers.vec.bin` — per-tier records, format owned by
  [`memory/vector/`](./vector-store.md).

`TierRecord` is the unit: `{id, content, importance, created_at,
session_id, source_path, score, memory_type}`. Public ops: `save`,
`get`, `delete`, `search_vectors`, `list`, `update_score`.

### Search (`search.rs`)

* **`search(store, query_vector, tier, limit) -> Result<Vec<Hit>>`** —
  vector-only top-K within one tier. Returns `Hit` with all the record
  fields plus the cosine similarity. Tier is optional; `None` searches
  all four.
* **`search_logged(...)`** — identical, but appends to `miss_log`
  whenever a query returns fewer than the requested limit. Used by
  retrieval-quality telemetry.
* **`search_with_graph(store, client, query_vector, tier, vector_k,
  graph_opts) -> HybridResult`** — the hybrid path. Four steps:
  1. Vector top-K via `search`.
  2. Graph expansion via `memgraph::expand_by_graph` (entities → multihop
     → chunks).
  3. `merge_and_rank` to fuse both result sets.
  4. Return `{vector_hits, graph_hits, ranked}`.

The caller pre-embeds; `query_vector` is a `&[f32]`.

### Merge & rank (`merge.rs`)

This is the algorithm contributors tweak most. The full logic in plain
terms:

* **Pure vector hit** (chunk found by vector only):
  `combined_score = COMBINED_ALPHA * similarity`. Source =
  `"vector"`.
* **Pure graph hit** (chunk found by graph only):
  `combined_score = (1 - COMBINED_ALPHA) * graph_similarity`. Source =
  `"graph"`.
* **Agreement hit** (chunk found by both — same `id` in both lists):
  `weighted_pair = COMBINED_ALPHA * existing_vec_sim + (1 - COMBINED_ALPHA) * graph_sim`.
  `combined_score = max(existing_combined, weighted_pair) + AGREEMENT_BONUS`.
  Source = `"vector+graph"`.

Constants:

* **`COMBINED_ALPHA = 0.6`** — weight on the vector signal. Higher α
  means literal similarity matters more; lower α means graph
  relationships matter more.
* **`AGREEMENT_BONUS = 0.05`** — the nudge that promotes agreement
  hits. Deliberately small — it shifts ties without overwhelming a
  strongly-matched solo hit.

Sort descending by `combined_score`; trim to `limit`.

The output rows are JSON values matching the Python tool envelope, so
the caller can drop them straight into the `results` field.

### Miss log (`miss_log.rs`)

Three append-only files for retrieval analytics:

* `misses.jsonl` — every query that returned fewer than `limit` hits.
* `feedback.jsonl` — every `rag.feedback` outcome the user / model
  recorded.
* `chunk_usage.json` — per-chunk citation counts (consumed by
  `rag.chunk_usage` to identify high-value chunks).

A process-wide `IO_LOCK` serialises writes. Files are jsonl-append, not
atomically rewritten — durability is good, transactionality is none.

### Prune (`prune.rs`)

`prune_rows(filters) -> PruneResult` is the filtered destructive
cleanup. Filters: `before_ts` (drop rows older than this), `memory_type`
(drop rows of this type), `score_lt` (drop rows below this score).
Filters compose with AND.

`preview(filters) -> Vec<TierRecord>` is the dry-run — returns matches
without deleting. The `rag.prune` LLM tool defaults to dry-run; the
LLM has to explicitly pass `confirm: true` to actually delete. The tier
gate (destructive: true) means only `destructive_tool_access` tier
can run the destructive form.

### Feedback (`feedback.rs`)

`record_outcome(chunk_id, outcome)` writes graph edges:

* Outcome = "ok" → `CITED_IN` edge from chunk to the answer-context
  (boosts the chunk's relevance for future queries with similar
  topics).
* Outcome = anything else → `RETRIEVAL_MISS` edge + a miss-log marker
  (signals the chunk was retrieved but didn't help).

These edges feed back into ranking on the next similar query. The
loop closes.

### Ingest (`ingest.rs`)

`trigger_ingest(workspace, paths)` is the N8N webhook trigger for
batch indexing. **HTTP transport is stubbed today** —
returns `{ok: false, error: "transport_not_wired"}`. A future slice
swaps in `reqwest::Client::post`. The signature is final-shape; the
swap is a one-file change.

Webhook URL composed from `WYLDE_N8N_BASE_URL` +
`WYLDE_N8N_INGEST_WEBHOOK` env vars.

### Wire surface (`actions.rs`)

Eight tools, all active in the registry:

* `rag.ask` (read) — hybrid retrieval. Requires `query_vector` until
  Rust embedder lands; returns `insufficient_context` otherwise.
* `rag.index` (destructive) — N8N trigger (transport-deferred).
* `rag.reindex` (destructive) — same.
* `rag.prune` (destructive) — dry-run by default.
* `rag.feedback` (read) — record an outcome.
* `rag.misses` (read) — read the miss log.
* `rag.chunk_usage` (read) — citation counts.
* `rag.graph_stats` (read) — calls memgraph `stats()`.

## How to extend

### Add a new tier

E.g. a `reference` tier for "external citations I want to be able to
find but not weight as highly":

1. Add the constant to `tiers.rs`:
   ```rust
   pub const TIER_REFERENCE: &str = "reference";
   pub const ALL_TIERS: &[&str] = &[
       TIER_CORE, TIER_EPISODIC, TIER_SEMANTIC, TIER_PROCEDURAL, TIER_REFERENCE,
   ];
   ```
2. `store.rs::TieredStore` will pick the new tier up automatically
   (it iterates `ALL_TIERS`).
3. Decide if you want a tier-specific scoring tweak — if so,
   `search.rs::search` is the right place to apply it.
4. Update docs (this file, [index.md](./index.md)).
5. Coordinate with Python — `Core/harness/memory/rag.py` has its own
   tier list; both sides must agree for the strangler-fig to work.

### Tune `COMBINED_ALPHA`

`COMBINED_ALPHA` is currently 0.6. Some scenarios:

* **0.8** — vector dominates. Use when your graph is sparse or noisy
  and the literal similarity is what you trust.
* **0.4** — graph dominates. Use when the graph encodes high-signal
  relationships (e.g. you've curated entity links) and similarity is
  the noisy signal.
* **0.5** — equal weight. Default-ish; consider when neither side is
  obviously stronger.

Changing α changes user-visible retrieval ordering. **Don't change it
without a parity gate against the Python implementation** — the two
sides have to stay in lockstep through the strangler flip.

### Tune `AGREEMENT_BONUS`

Currently 0.05. Effect: pushes "found by both" above narrowly-tied
"found strongly by one." Increasing it (say to 0.15) makes agreement
much more important; decreasing it (to 0.01) makes the agreement
signal almost cosmetic.

Don't push above ~0.20 — at that point an agreement hit with mediocre
scores on both sides can beat a near-perfect single-side hit. That's
usually wrong.

### Add a new retriever

Currently two retrievers feed `merge_and_rank`: vector and graph.
Suppose you add a third — e.g. an **LLM-judged** retriever that asks
a cheap LLM "is this chunk relevant?":

1. Define a `JudgeHit` type in your new `judge.rs` module.
2. Extend `merge_and_rank` to accept a third hit slice:
   ```rust
   pub fn merge_and_rank(
       vector_hits: &[Hit],
       graph_hits: &[GraphHit],
       judge_hits: &[JudgeHit],
       limit: usize,
   ) -> Vec<Value>
   ```
3. Pick weights: maybe `COMBINED_ALPHA_VECTOR = 0.5,
   COMBINED_ALPHA_GRAPH = 0.3, COMBINED_ALPHA_JUDGE = 0.2`, summing
   to 1.0.
4. Define agreement bonuses: pairwise + three-way.
5. Wire into `search_with_graph` (and a new `search_with_judge_and_graph`).
6. Heavy parity work — Python will need the same retriever for any
   strangler test.

### Add a new outcome kind

`record_outcome` today writes `CITED_IN` or `RETRIEVAL_MISS`. Suppose
you want a `MARKED_HARMFUL` outcome for when the user flags a chunk
as misleading:

1. Add a `MarkedHarmful` variant to whatever enum represents
   outcomes in `feedback.rs`.
2. Decide the graph edge type — e.g. `MARKED_HARMFUL_BY` from chunk
   to a User node.
3. Add a write path: `mark_chunk_harmful(chunk_id, user_id)`.
4. Decide if `merge_and_rank` should down-weight harmful chunks —
   probably yes; add a `harmful_penalty` step before the bonus.
5. Expose as a `rag.mark_harmful` LLM tool.

### Wire the N8N HTTP ingest transport

`ingest.rs::trigger_ingest` is signature-final, transport-stubbed.
Land the transport:

```rust
let client = reqwest::Client::new();
let resp = client.post(&webhook_url)
    .json(&payload)
    .send()
    .await?;
if resp.status().is_success() {
    Ok(json!({"ok": true, "execution_id": parse_execution_id(resp).await?}))
} else {
    Ok(json!({"ok": false, "error": format!("n8n_returned_{}", resp.status())}))
}
```

Add `reqwest` to `Cargo.toml` (it's not pulled in today). Make sure
egress is allowed via the gateway allowlist — N8N's webhook URL needs
to be in the egress capability list.

## Gotchas

* **The model has to pre-embed.** `rag.ask` requires a `query_vector`
  in the args today. Until the Rust embedder ports (Phase 7.D), the
  model has to either route through Python (canonical) or skip the
  tool. The error envelope is `insufficient_context`; treat it as
  expected, not surprising.
* **The N8N transport is stubbed.** `rag.index` and `rag.reindex`
  return `status: "deferred"` until the HTTP client lands. Don't
  build features on them without checking.
* **`merge_and_rank` constants are parity-gated.** Changing
  `COMBINED_ALPHA` or `AGREEMENT_BONUS` on the Rust side without a
  matching Python change means the strangler flip breaks ordering for
  every user. Coordinate.
* **Miss log is append-only.** It grows forever unless something
  prunes it. There's no built-in rotator today; consider adding one
  before it crosses 100 MB.
* **`rag.prune` defaults to dry-run.** The LLM has to pass
  `confirm: true` to actually destroy. The tier gate enforces
  destructive-tier access on top of that. Don't loosen either gate
  without understanding what you're enabling — the prune filters are
  flexible enough to delete every memory of a tier with one bad
  request.
* **Agreement hits inherit the better of the two scores, plus the
  bonus.** This means an agreement hit *can't* be worse than the
  better solo retriever's score for the same chunk. If your goal is
  "demote agreement hits unless the agreement is strong," the bonus
  is the wrong knob — you'd need to change the formula to
  `min(...)` (penalising disagreement) instead. Think carefully.
* **`session_id` is opaque text, not a graph reference.** RAG records
  carry the session id as a string; there's no Session node in
  memgraph. If you need to query "all chunks from this session,"
  you scan the JSON or add an index — don't add a session node and
  edges without a real use case.

## Cross-links

* [index.md](./index.md) — memory subsystem overview.
* [vector-store.md](./vector-store.md) — the substrate behind each
  tier's vector mirror.
* [memgraph.md](./memgraph.md) — the graph retriever that
  `search_with_graph` consults.
* [long-term.md](./long-term.md) — similar scoring concepts,
  separate store.
* `Core/harness/memory/rag.py` — Python canonical (until the flip).
* `~/.claude/projects/.../memory/wylde_phase7b_rag_shipped.md` — the
  Phase 7.B-3 ship report.

---

*RAG is where most retrieval magic happens. The four-tier model and
the merge_and_rank algorithm are the load-bearing decisions; touch
them only with a parity gate and an open eye for downstream
consequences.*
