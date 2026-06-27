# Lexical (BM25) retrieval + RRF fusion

A per-workspace lexical recall signal fused with the existing dense (cosine)
retrieval. **Default OFF** — with the master toggle off, retrieval is
byte-for-byte the dense-only path that shipped before, and no lexical index is
even created. Turning it on adds an exact-token signal (rare identifiers, error
codes, literal names the embedder blurs) without replacing the brute-force vector
scan and without a second copy of the chunk bodies on disk.

Implemented in `wylde-workspaces`:
`src/rag/lexical_config.rs`, `src/rag/indexer/{lexical,fuse}.rs`,
`src/rag/lexical_eval.rs`, and the fusion wiring in `src/rag/indexer/search.rs`.

## The toggle

`LexicalConfig` mirrors `RoutingConfig` exactly: a process-global cache seeded
from `<data_dir>/settings/lexical.json`, **fail-closed to OFF** on any
missing/corrupt/malformed input, read in-process by the RAG search hot path. The
GUI writes through the `settings.lexical.{get,set}` facade verbs on the
`wylde-workspaces` pipe (partial-patch on set).

| knob | default | role |
|---|---|---|
| `enabled` | `false` | master toggle — OFF ⇒ identity with today |
| `rrf_k` | `60` | RRF rank-bias constant `w / (k + rank)` |
| `w_dense` / `w_lex` | `1.0` / `1.0` | per-arm RRF weights |
| `min_bm25` | `1.0` | BM25 floor for the on-topic gate (the low-cosine bypass) |
| `fused_relative_floor` | `0.6` | relative dynamic-k floor on the fused score |
| `active_file_focus_boost` | `≈0.0083` | exact-open-file focus lift (RRF scale) |
| `active_file_dir_focus_boost` | `≈0.0033` | same-directory focus lift |

The fusion knobs are **provisional** until the eval sweep calibrates them against
the live index (see *Eval* below), exactly as `RoutingConfig::abs_threshold` was
0.50 → 0.62 in routing R4. They only bite when `enabled` is `true`.

## The index

A tantivy inverted index at `<data_dir>/workspaces/<id>/index/lexical/` — inside
the **same `index/` bundle** as the vectors, so workspace-delete removes it and
its temp/merge segments inherit the same OS ACLs (never a global temp). One doc
per chunk:

| field | type | stored? | purpose |
|---|---|---|---|
| `chunk_id` | `STRING` | **STORED** | join key back to the loaded `IndexedChunk`; the delete term for incremental sync |
| `path_raw` | `STRING` | indexed | exact-path delete + exact-path match |
| `path_text` | `TEXT` | indexed | tokenised path/identifier BM25 |
| `content` | `TEXT` | indexed, **NOT stored** | the BM25 body — term postings only, no second body copy |

tantivy is pure-Rust, needs **no `protoc`**, and builds clean on Windows — the
explicit contrast with the rejected `lancedb` (`store.rs:13`). It adds a *signal*,
not a storage-engine swap (ANN stays out).

### Built from chunks, never a fresh walk

The index is built from the persisted chunk set, so it inherits the
`ExclusionMatcher` hygiene and the content-hash manifest for free and **can never
drift from `chunks.jsonl`** — it is structurally incapable of indexing a
`target/` artifact the vector index skipped.

- **Full reindex** rebuilds `lexical/` from the same chunk slice (under the index
  lock, after the manifest write).
- **Backfill**: flipping the toggle on with an already-indexed workspace builds
  the lexical index once from the existing chunks — **no embedder, no Ollama** (so
  seconds, not the ~6 min a re-embed costs). It is a true switch, not a re-index
  event.
- **Watcher delta** keeps it in step per file: exact-path delete + add on upsert,
  exact-path + subtree-id delete on remove. `delta == full` (both converge to the
  same index over the same final chunk set).

All sync is **gated on the toggle and best-effort** — a tantivy failure is
logged, never fatal to the vector index (mirrors the graph half). A lexical hit
whose `chunk_id` has no matching loaded chunk is silently dropped at search time,
so **lexical can never surface a chunk the vector store lacks**.

> Known limitation: toggling OFF, mutating files, then ON before a full reindex
> can leave the lexical index missing the interim changes — a soft *under-recall*
> (never wrong content; the chunk_id join guarantees that), which self-heals on
> the next full reindex.

## Fusion (the read path)

When ON, `query_with_vec` runs both arms over the loaded chunks and fuses them
before today's levers:

```
dense:   cosine(query, chunk) ∀chunk → descending rank
lexical: BM25 over lexical/ (user text + boosted anchor sub-query) → rank, joined by chunk_id
fuse:    fused[c] = w_dense/(rrf_k+rank_dense[c]) + w_lex/(rrf_k+rank_lex[c])
         + active-file focus boost (RRF scale)
         → dynamic-k cutoff → MMR diversity → SearchHit
```

`SearchHit.score` **stays the true cosine** (the GUI/IPC contract); the RRF score
and any BM25 hit ride in additive `fused_score` / `lexical_score` fields, omitted
from the JSON when absent so the OFF shape is unchanged.

### Dynamic-k under fusion (the one friction)

RRF scores are scale-free, so the absolute *cosine* floor cannot be applied to a
fused score — but its purpose ("off-topic injects nothing") is preserved by an
**on-topic gate on the top candidate**: it is on-topic if its dense cosine clears
`MIN_ABSOLUTE_SCORE` **OR** a BM25 hit clears `min_bm25`. A strong exact-token hit
at low cosine therefore **passes the gate and is kept — that is the recall win** —
while a query off-topic to *both* arms still injects nothing. The fused-sorted
prefix is then trimmed by `fused_relative_floor · top_fused`.

### Anchors

Resolved `[anchors: …]` terms become a **boosted BM25 sub-query** (folded into the
lexical arm), replacing the legacy substring `contains` boost: IDF weighting (a
rare identifier outscores a common word) and exact token boundaries (`add` no
longer matches inside `address`) fall out for free, and the path field is boosted
highest so the symbol's *defining* file ranks top. The OFF path keeps the legacy
substring boost verbatim. Active-file stays an additive *focus* boost, separate
from the lexical relevance arm.

## Eval

`src/rag/lexical_eval.rs` runs three arms — **dense**, **lexical**, **fused** —
over a corpus, graded against a gold set with a **lexical class** (exact tokens
the embedder blurs) and a **semantic class** (the no-regression guardrail), reusing
the pure metrics from `wylde_concept_routing::eval`. The CI mechanism proof
(synthetic, deterministic) shows lexical-class recall rising dense → fused while
semantic-class recall holds (`fused ≈ dense`).

The live measurement is the `#[ignore]`d driver — run it by hand:

```text
cargo test -p wylde-workspaces --test lexical_eval -- --ignored --nocapture
```

It reads the live `chunks.jsonl`, builds a scratch BM25 index (never touching the
live data), embeds a draft gold set against the running Ollama, runs the harness +
the `rrf_k`/weights sweep + the relative-floor calibration, and writes
`outputs/lexical-bm25-eval-results.md`. The sweep tables are what retune the
provisional defaults above.
