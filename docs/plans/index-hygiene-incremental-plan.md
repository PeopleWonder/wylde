# Index Hygiene + Incremental Indexing — Design (read-only scoping pass)

**Date:** 2026-06-22 · **Branch:** `feat/thought-bubble-system` · **Status:** ✅ **COMPLETE — all four phases shipped**
**Crate:** `rust/crates/wylde-workspaces` (+ `wylde-concept-routing`) · **Author:** scoping pass for Aaron

> ## ✅ BUILD STATUS — index hygiene plan COMPLETE (2026-06-22)
> All four locked pieces are implemented, tested, and merged to trunk:
> - **P1 — Walk-time exclusion** (`ExclusionMatcher`, gitignore + deny-list + `.wyldeignore`) — shipped `6e83a4f`.
> - **P2 — One-time purge** (`purge_excluded`, filter-only) — shipped `6e83a4f`.
> - **P3 — Content-hash manifest** — NEW: `rag/indexer/manifest.rs` (`manifest.json`: version +
>   embed_model/dim guard + per-file sha256 + `(mtime,size)` fast-path + chunk-ids) +
>   `rag/indexer/lock.rs` (per-workspace write-lock). `reindex_delta` is now a pure, hash-driven
>   diff (metadata walk → hash only on `(mtime,size)` drift → re-embed only changed/new, drop
>   deleted, keep unchanged); chunks-first/manifest-second atomicity under the lock; an incompatible
>   manifest (model/dim/version) forces a full rebuild; the watcher's `upsert_file` short-circuits a
>   no-op save by hash. A pre-P3 index upgrades in place via an mtime fallback (no mass re-embed).
> - **P4 — Concept stability** — NEW: greedy nearest-centroid **carry-over** of `sem:` ids (cosine ≥
>   `SemanticParams::carry_over_threshold`, default **0.85**) + a persisted never-reused ordinal
>   allocator (`concepts/identity.rs`, `concept_identity.json`); a `dangling: bool` serde-default on
>   `Relation` (migration-free) + `relations_bridge::sweep_dangling` — an edge to a vanished concept
>   is **flagged + surfaced + excluded from routing, never deleted**; `dangling_count` in build /
>   `relations.graph` replies.
>
> Verified: 466 `wylde-workspaces` lib tests + 89 `wylde-concept-routing` lib tests green; the
> mock-embedder e2e delta-reindex integration test green; clippy + workspace build clean. Live
> A/B against a real index deferred (no dev memgraph/ollama running — same precedent as the rest of
> TBS).

> Context: R4 found the live workspace RAG index is **~58 % build artifacts** (rustdoc
> HTML under `*/target-dev/doc`). This is the #1 lever for concept-routing quality and it
> hurts plain RAG too. Aaron has **locked the fix shape**; this doc traces the current
> implementation (with `file:line` citations), designs the four locked pieces concretely,
> and lays out a phased build plan. Implementation is a later pass.

---

## 0. Root cause — confirmed on disk

The indexer walks the **raw filesystem** and does **not** honor `.gitignore`. The walker
prunes a hard-coded set of directory *names* by **exact match**:

- `walk.rs:31-49` — `SKIP_DIR_NAMES` contains `"target"`, `"node_modules"`, `"dist"`,
  `"build"`, `".git"`, … matched by `SKIP_DIR_NAMES.contains(&name.as_str())` (`walk.rs:103`).
- The dev build tree is **`target-dev`**, not `target`. Exact-name match means
  `target-dev/` is **never pruned**.
- `SKIP_SUFFIXES` (`walk.rs:24-28`) has no `html`/`htm`, so rustdoc HTML (text, < 1 MB)
  sails through the binary sniff (`walk.rs:153`) and the size cap (`walk.rs:140`).

Measured this pass:

```
1029  *.html under Core/GUI/target-dev/doc      (e.g. doc/settings.html, doc/src/.../*.rs.html)
       more under rust/target-dev
```

Both dev trees ARE gitignored — but by **nested** `.gitignore` files the walker can't see:

```
Core/GUI/.gitignore:5   target-dev/
rust/.gitignore:7       target-dev/
```

> ⚠️ The **root** `.gitignore` ignores `target/` and `rust/target/` (exact) — it does **not**
> list `target-dev`. So "honor `.gitignore`" only fixes this because the `ignore` crate
> reads nested `.gitignore` files at every level. A naive root-only check would still miss
> it. This shapes the design: **gitignore-honoring must be the real nested-aware engine, and
> the built-in deny-list backstop must use a `target*` glob** (not just `target`).

`ignore` crate availability: **not present** in `Cargo.lock` today (no transitive copy).
Adding it pulls `globset`, `walkdir`, `same-file`, `regex-automata` (`memchr` already in-tree
via `nucleo-matcher`).

---

## 1. Current-state trace

### 1.1 The walk + chunk (`rag/indexer/walk.rs`)
- `walk_and_chunk(folder)` `walk.rs:71` → hand-rolled recursive `std::fs::read_dir`
  (`walk_dir` `walk.rs:81-111`). No gitignore awareness; prune logic is the exact-name
  `SKIP_DIR_NAMES` + hidden-dotfile skip (`walk.rs:93`) + `SKIP_SUFFIXES`.
- `chunk_file` `walk.rs:125` — per-file: suffix skip, size cap 1 MB (`MAX_INDEXABLE_BYTES`
  `walk.rs:13`), NUL-byte binary sniff, UTF-8 lossy, then `chunk_text` (4 000-char windows,
  200 overlap, `walk.rs:14-19`). Path stored canonicalised (`canonical_path` `walk.rs:184`).
- `is_indexable_path(root, path)` `walk.rs:206` — **the watcher's path-only pre-filter**,
  a *second, hand-duplicated* copy of the same skip-dir/hidden/suffix logic. Today this is
  the only thing that keeps the watcher from upserting an artifact file — and it has the
  **same `target` vs `target-dev` blind spot**.

### 1.2 The index pipeline (`rag/indexer/mod.rs`)
- `reindex(def)` `mod.rs:57` → `reindex_full` if no index (`has_index`), else `reindex_delta`.
- `reindex_full` `mod.rs:66` — walk → graph-write → `embed_chunks` → `persist`.
- `reindex_delta` `mod.rs:104` — **re-walks the whole folder every pass**, re-ingests the
  whole graph, then `plan_delta` `mod.rs:163` does an **mtime-based** keep/re-embed split
  (a file is "changed" when walked mtime > newest cached chunk mtime, 1 ms tolerance
  `mod.rs:179`; "gone" when cached but not walked). So **incremental-by-mtime already
  exists**, but there is **no content hash** and **no manifest** — mtime is the only signal.
- `embed_chunks` `mod.rs:229` → `crate::embeddings::embed(texts)`.
- `chunk_id` `mod.rs:263` = `sha256(path::chunk_idx::mtime)[..16]` — **mtime is baked into
  the id**, so any mtime change re-keys every chunk of that file (relevant to delta + purge).

### 1.3 Persistence (`rag/indexer/store.rs`)
- `<data_dir>/workspaces/<id>/index/{chunks.jsonl, rag_state.json}` (`store.rs:88-98`).
- `IndexedChunk` `store.rs:38` = `{id, path, chunk_idx, content, mtime, start_line,
  end_line, vector}`. `save_chunks` `store.rs:121` rewrites the **whole** JSONL atomically
  (tmp + rename). `RagState` `store.rs:66` = indexing flag + counts + last_error.
- **No manifest file.** The implicit "manifest" is the chunks' own `(path, mtime)`.

### 1.4 The file watcher (`watcher/`)
- `watcher/mod.rs` — one watcher, the **active** workspace only; notify → debounce
  (500 ms default, `DEFAULT_DEBOUNCE_MS` `mod.rs:58`) → per-file delta dispatch. Already
  **incremental per file**.
- `notify.rs:translate` `notify.rs:39` folds events → `Upsert`/`Remove`.
- `delta.rs` — `upsert_file` `delta.rs:63` / `remove_file` `delta.rs:115`. Upsert path:
  `is_indexable_path` gate → `chunk_one_file` → graph clear+rewrite → `vector_upsert`
  (`delta.rs:149`: load all chunks, `retain(path != canonical)`, extend fresh, **full
  rewrite**). So the watcher already does targeted per-file vector updates; it just lacks a
  hash check (re-embeds on any event that survives debounce) and shares the walker's
  exclusion blind spot.

### 1.5 Embedding pacing/pooling (`embeddings.rs`) — already shipped
- `embed(texts)` `embeddings.rs:165` splits into ≤ `EMBED_MAX_BATCH` = 64 batches
  (`:129`; runner crashes > ~255 inputs). A **large index** (> `LARGE_INDEX_MIN_INPUTS` =
  1500 inputs `:104`) is **paced** to `EMBED_BATCH_MIN_PERIOD_MS` = 6000 ms/batch (`:97`)
  to keep Ollama's runner-socket churn under the Windows ephemeral-port pool. Concurrency
  capped at 4 via a process-wide semaphore (`:49`). All env-overridable.
- **Implication for incremental:** smaller deltas (< 1500 inputs) never pay the 6 s pace —
  they're already fast. Incremental indexing's whole value is keeping passes *under* that
  threshold. The pacing layer needs **no change**; the manifest just makes the large-pace
  path engage rarely (only true full rebuilds / the one-time purge re-embed, if any).

### 1.6 Concepts — clustering, IDs, persistence, relations
- **Data model** `concepts/concept.rs:62` — `Concept{id, label, description, members,
  member_files, parent_concepts, described_by, centroid, source, …}`. Source enum
  `concept.rs:39`: `DirectoryCluster` / `Embedding` / `Manual`.
- **Directory concepts** (`concepts/cheap.rs`) — id = `dir:<cluster path>`
  (`DIR_CONCEPT_PREFIX` `cheap.rs:31`, `cheap.rs:67`). **Path-derived ⇒ stable** across
  rebuilds (changes only if a directory is renamed/removed).
- **Semantic concepts** (`concepts/semantic.rs`) — `build_semantic_concepts` `semantic.rs:68`
  runs spherical k-means (`clustering.rs`), then assigns id = `sem:{ci:04}`
  (`SEM_CONCEPT_PREFIX` `semantic.rs:39`, format at `semantic.rs:107`) where **`ci` is the
  soft-cluster ordinal index**. Seed is fixed (`0x5EED` `semantic.rs:60`) ⇒ deterministic
  for the *same* corpus. **But `ci` is NOT stable across a changed corpus** — over a
  different/grown chunk set, k-means re-partitions and `sem:0003` can map to an entirely
  different theme (or vanish if `k` shrank). Fixed-seed determinism ≠ cross-recompute
  identity. **This is the Option-A stability bug.**
- **Persistence** (`concepts/store.rs`) — `concepts.json`, encrypted-at-rest (OI-14), one
  JSON array. `replace_all` `store.rs:80` swaps the set. The build verb's `finish_build`
  (`api.rs:131`) **preserves only `Manual` concepts** and replaces the rest.
- **Build is on-demand already** — verbs `workspaces.concepts.build` (`api.rs:83`) and
  `build_semantic` (`api.rs:110`). The **watcher delta path never touches `concepts.json`**
  — it only updates chunks + graph. So "chunks continuous, concepts on demand" (Option A) is
  **already the architecture**; the only gaps are stable IDs + orphan handling.
- **Relations** (`concepts/relations_bridge.rs`) — `concept_relations.json`, edges are
  `NodeRef::Concept{id}` / `NodeRef::Vocab{identifier}` (`relations_bridge.rs:68-75`).
  **Validation happens only at add-time** (`node_exists` `relations_bridge.rs:68`); there is
  **no sweep** when a recompute drops a concept id, so a relation keyed on a vanished/reused
  `sem:NNNN` silently dangles or — worse — re-points to a different theme that inherited the
  ordinal. Relation types live in the isolated `wylde-concept-routing` crate (serde-shaped),
  so adding a serde-default field there is migration-free.

---

## 2. Design 1 — Walk-time exclusion (three layers)

**Goal:** the walk indexes only real source, on **both** the full walk and the incremental
watcher path, and the decision lives in **one** predicate.

### 2.1 The shared predicate — `ExclusionMatcher`
Introduce one struct, built once per workspace root, consulted by both paths:

```
// rag/indexer/exclude.rs  (new)
pub struct ExclusionMatcher { /* compiled gitignore stack + .wyldeignore + deny-list */ }
impl ExclusionMatcher {
    pub fn for_root(root: &Path) -> Self;                 // build once
    pub fn is_excluded(&self, path: &Path, is_dir: bool) -> bool;
}
```

**Recommended integration (low-churn):** keep the existing recursion + chunker in `walk.rs`;
just **consult the matcher** in `walk_dir` (prune a dir when `is_excluded(dir, true)`, skip a
file when `is_excluded(file, false)`) and in `is_indexable_path`. Do **not** rip out the
hand walker for `ignore::WalkBuilder` — that would re-do chunking/canonicalisation plumbing
for no extra correctness, and the matcher-consult gives the two paths byte-identical
behaviour for free.

Build the matcher with the `ignore` crate's matcher API (not the full walker):
- `ignore::gitignore::GitignoreBuilder` for the **nested** `.gitignore` stack: walk from
  `root` down, `add` each `.gitignore`; also `add` ancestor `.gitignore`s (parents) so a
  workspace that is a subdir of a git repo still honors the repo's rules. Set the equivalent
  of `require_git(false)` semantics so `.gitignore` files are honored even when `root` is not
  itself a git work tree.
- A **second** `Gitignore` built from in-memory glob strings = the **built-in deny-list**
  (layer b).
- A **third** matcher for `.wyldeignore` (layer c), built the same way as the gitignore
  stack but with filename `.wyldeignore`.

`is_excluded(path)` = `deny_list.matched(path)` OR `gitignore.matched(path)` OR
`wyldeignore.matched(path)` (see precedence, §2.5). `.git/` is always excluded
unconditionally.

### 2.2 Layer (a) — honor `.gitignore` + skip `.git/`
Nested-aware via the gitignore stack above. This is the **root-cause fix**: in this repo it
excludes `Core/GUI/target-dev/` and `rust/target-dev/` because their nested `.gitignore`s
list `target-dev/` (`Core/GUI/.gitignore:5`, `rust/.gitignore:7`) — which the current
exact-name `target` skip misses.

### 2.3 Layer (b) — built-in artifact deny-list (backstop)
Independent of whether the workspace has any `.gitignore`. Conservative defaults:
- **Dirs:** `.git`, `target`, **`target-*`** (glob — the thing that bit us), `node_modules`,
  `dist`, `build`, `out`, `.next`, `.svelte-kit`, `__pycache__`, `venv`, `.venv`, `env`,
  `.env`, `.tox`, `.mypy_cache`, `.pytest_cache`, `.ruff_cache`, `.idea`, `.vscode`,
  `.wylde`, `.git`.
- **File globs:** `*.min.js`, `*.min.css`, `*.map`; lockfiles by name — `Cargo.lock`,
  `package-lock.json`, `yarn.lock`, `pnpm-lock.yaml`, `poetry.lock`, `uv.lock`,
  `composer.lock`, `Gemfile.lock`.
- **Keep** the existing binary `SKIP_SUFFIXES`.
- **Do NOT** add `html`/`htm` to the suffix ban — the rustdoc problem is a *directory*
  problem (`target*/doc`), already covered by the `target-*` dir rule + gitignore. Banning
  `.html` would wrongly exclude legitimate HTML-content workspaces. (Decision §6.3.)

The generated-doc concern reduces to "doc dirs under a build tree", which the `target-*`
rule covers. We deliberately do **not** deny a top-level `docs/` (usually real prose).

### 2.4 Layer (c) — optional `.wyldeignore`
Gitignore-syntax file at the workspace root (and nested). Per-workspace user override,
including `!`-re-includes. Honored via its own matcher in the predicate.

### 2.5 Precedence (decision §6.2)
**Recommended v1:** the built-in deny-list is a **hard backstop** — if it matches, the path
is excluded regardless of a `.wyldeignore !` re-include — **except** that `.git/` is always
out and nothing can re-include it. Rationale: simplest, safe, and the "I genuinely want to
index a `build/` dir" case is rare; we can relax later. (Alternative: let `.wyldeignore !`
win over the deny-list for everything but `.git`.)

### 2.6 Applies to BOTH passes
- Full walk: `walk_dir` consults the matcher (prune dir → no descent; skip file).
- Incremental: `is_indexable_path` (`walk.rs:206`) delegates to the same matcher. The matcher
  is built once and **cached on the active watcher** (`ActiveWatcher`, `watcher/mod.rs:336`),
  rebuilt when a `.gitignore`/`.wyldeignore` change event is observed (the watcher already
  sees those events; treat them as a matcher-invalidation trigger).

### 2.7 Safety valve — dry-run diagnostic
Add a read-only `workspaces.rag.walk_preview` (or a flag on reindex) returning
`{would_index: n, would_exclude: m, sample_excluded: [...]}` so we can confirm the matcher's
effect **before** committing to a purge. Low effort, de-risks over-exclusion.

---

## 3. Design 2 — Incremental indexing via a content-hash manifest

**Goal:** reuse the persisted index across passes; re-chunk+re-embed only changed/new files,
drop deleted files' chunks, keep unchanged vectors — driven by **content hash**, not just
mtime. Watcher-driven; full rebuild becomes rare/explicit.

### 3.1 Manifest schema — `index/manifest.json` (new, alongside chunks.jsonl)
```jsonc
{
  "version": 2,                         // bump to force a one-time migration/purge (§5)
  "embed_model": "nomic-embed-text",    // model/dim guard — see §3.4
  "embed_dim": 768,
  "files": {
    "<canonical_path>": {
      "hash": "<sha256(file bytes), hex[..16]>",
      "size": 12345,
      "mtime": 1700000000.0,
      "chunk_ids": ["abcd...", "ef01..."],   // the IndexedChunk.ids this file owns
      "chunk_count": 2
    }
  }
}
```
Keyed by the **same canonical path** chunks use (`walk::canonical_path` `walk.rs:184`), so
manifest ↔ chunks lookups always agree (the same discipline the watcher already relies on).

### 3.2 Diff (pure, testable) then apply
1. **Walk** (gitignore-aware, §2) → live `(path, mtime, size)` set — *no content read yet*.
2. Per live file:
   - manifest has it **and** `(mtime, size)` match → **UNCHANGED** (keep its chunks; cheap,
     no read). ← the fast path that makes most passes nearly free.
   - else read bytes + hash:
     - hash == manifest hash → **UNCHANGED-touched** (refresh mtime in manifest only; **no
       re-embed**). ← fixes the clone/checkout/`touch` false-positive that mtime-only
       (`plan_delta` `mod.rs:163`) re-embeds today.
     - hash differs → **CHANGED**; new path → **NEW**. Enqueue for chunk + embed.
3. manifest path ∉ live set → **DELETED** → drop that file's `chunk_ids` from chunks.jsonl +
   graph-clean (reuse `BoltClient::delete_file_nodes`, as `delta.rs:115` does).
4. Embed only CHANGED/NEW (via `embeddings::embed` — batching/pacing unchanged) → merge with
   kept chunks → write.

This generalises the existing `plan_delta` (keep mtime as the **fast pre-check**, add hash as
the **confirm step**). The watcher's per-file `upsert_file` does the single-file version: hash
the one file, compare to its manifest entry, skip re-embed when equal (saves an Ollama
round-trip on no-op saves).

### 3.3 Atomicity / interrupted-update consistency
**Invariant: chunks.jsonl is written (tmp+rename) FIRST; manifest.json SECOND.**
- Crash *between* → manifest is **stale/behind** the chunks. Next pass sees `(mtime,size)`
  mismatches for already-embedded files and re-embeds them — wasteful but **correct**
  (idempotent: same `chunk_id` for same `(path, idx, mtime)`).
- The reverse (manifest ahead of chunks) would *skip* needed embeds → missing vectors. The
  ordering makes that impossible.
- **Write-lock:** a manual `reindex` can race watcher deltas (both rewrite chunks.jsonl +
  manifest). This race exists today for chunks.jsonl alone. Serialise both writes under a
  **per-workspace index mutex** (a `OnceLock<DashMap<String, tokio::Mutex<()>>>` or a lock
  file). The chunks+manifest pair must be written under one held lock so they never tear
  relative to each other.

### 3.4 Model/dim-change guard (closes a silent-corruption gap)
Today nothing detects an embed-model or target-dim change; a swap would silently mix
incompatible vectors in chunks.jsonl. Store `embed_model`+`embed_dim` in the manifest; on
load, if either differs from the current config (`embed_model()`/`embed_dim()` in
`common.rs`), **force a full rebuild** and log it. (Decision §6.7.)

### 3.5 Composition summary
- **Persistence:** manifest is additive next to `chunks.jsonl`/`rag_state.json`; the
  full-rewrite `save_chunks` stays (low-thousands of chunks).
- **Embed pacing:** unchanged; incremental keeps inputs/pass below the 1500-input large-pace
  threshold so deltas stay fast; only true full rebuilds / the purge re-embed pay the 6 s
  pace.
- **Watcher:** gains a hash check in `upsert_file`; otherwise the same notify→debounce→delta
  shape.

---

## 4. Design 3 — Concept stability (Option A)

**Locked model (already the architecture):** chunks update continuously (watcher); concepts
recompute **on demand** (`build`/`build_semantic` verbs), **not** per chunk update. The
missing pieces are (i) **stable IDs** so authored relations survive a recompute, and (ii)
**orphan handling** when a recompute drops a concept.

### 4.1 Make `sem:` IDs stable — greedy nearest-centroid carry-over
Today `sem:{ci:04}` is the cluster ordinal (`semantic.rs:107`) → unstable across a changed
corpus. Replace ordinal assignment with **identity carry-over**:

On recompute, given the new clusters (each with a centroid) and the **prior** semantic
concepts (each already carries its `centroid`, `concept.rs:90`):
1. Greedily match each new cluster to the most cosine-similar **prior** `sem:` concept
   (descending similarity, one-to-one).
2. If best cosine ≥ **τ (≈ 0.85, tunable)** → **reuse the prior id** (same theme, drifted) —
   update its centroid/members/label in place.
3. Else → **mint a new stable id** from a persisted monotonic allocator: `sem:<n>` where `n`
   = (max existing `sem:` ordinal) + 1, **never reused**. Persist `next_sem_ordinal` in the
   manifest (or a small `concept_identity.json`) so a deleted theme's number is never
   recycled onto a different theme.

Keep `build_semantic_concepts` pure by passing the prior concept set in (it's
unit-testable; the matcher is deterministic). `api.rs::finish_build` already loads existing
concepts to preserve `Manual` — extend it to also feed prior `Embedding` concepts to the
matcher. `disambiguate_labels` only edits **labels**, not ids (`semantic.rs:146`), so it's
unaffected. Directory concepts (`dir:<path>`) are already stable.

> Why not hash the centroid into the id? Centroids drift slightly between recomputes for the
> "same" theme, so a centroid-hash id would churn. Similarity-matching with a threshold is the
> standard stable-cluster-identity approach and tolerates drift.

### 4.2 Orphan handling — flag, never silently delete the user's edge
When a recompute **drops** a concept (no new cluster matches its prior id ≥ τ):
- **Do not delete** `concept_relations.json` edges keyed on it.
- Add a `sweep_dangling(workspace_id)` step (in `relations_bridge.rs`) that, after a
  recompute, marks every relation whose `NodeRef::Concept{id}` no longer resolves via
  `store::get` (and `Vocab` likewise) as **dangling**.
- Mechanism: add `dangling: bool` (serde `#[serde(default)]` ⇒ migration-free) — or a
  `dangling_since: f64` — to `Relation` in the isolated `wylde-concept-routing` crate. The
  router already treats an empty/absent relation graph as identity, and a dangling edge
  should be **excluded from routing** but **retained on disk** and **surfaced in the
  relations tree UI** for the user to re-point or delete.
- Report `{dangling_count}` in the `build`/`build_semantic` reply and in
  `relations.graph`/`relations.list` so the GUI can badge it.

When a recompute **adds** a concept: fresh stable id, no relations yet — nothing to do.

### 4.3 Optional nudge (not required for v1)
`workspaces.concepts.freshness` already exists (`api.rs:421`) and detects drift. It can drive
a "concepts look stale — rebuild?" hint and/or a periodic on-demand recompute. No new
mechanism needed.

---

## 5. Design 4 — One-time purge of the polluted index

The exclusion fix (§2) makes **new** indexes clean, but the existing `chunks.jsonl` keeps its
~58 % artifact chunks until something rewrites it.

**Recommended: filter-only purge (no re-embed) + concept re-cluster.**
1. `purge_excluded(def)`: load chunks → drop every chunk whose path satisfies the new
   `ExclusionMatcher::is_excluded` → if any dropped: `save_chunks(survivors)`, rebuild
   `manifest.json` from survivors, and graph-clean the dropped files
   (`BoltClient::delete_file_nodes` per dropped path / batched).
2. Then run `build_semantic` on the cleaned set so concepts re-cluster on real source — the
   **first post-purge build establishes the stable-id baseline** (§4.1).

No embedding calls needed (we keep the surviving vectors), so the purge is fast and does
**not** pay the 6 s/batch large-index pace.

**Rollout (decision §6.6):** drive it off the manifest `version`. On service boot / first
index op, if `manifest.json` is **absent or `version` < current**, run the purge once, then
write the bumped manifest. This self-heals existing installs with no manual step. Also expose
an explicit `workspaces.reindex_purge` verb for re-runs/diagnostics. (`workspaces.reindex`
`api.rs:252` already exists for a full re-embed if ever wanted, but that's the slow path.)

**Verification:** before/after chunk counts, and specifically the count of chunks whose path
is matcher-excluded: expect **~58 % → ~0**. Log it in the purge outcome
(`{dropped, kept, excluded_remaining: 0}`).

---

## 6. Phased build plan

Ordered so the **root-cause win lands first**, the live index is **cleaned next**, then the
robustness + stability layers.

| Phase | What | Why | Effort | Risk | Verify |
|---|---|---|---|---|---|
| **P1 — Walk-time exclusion** | `ExclusionMatcher` (gitignore + deny-list + `.wyldeignore`); consult in `walk_dir` + `is_indexable_path`; add `ignore` dep; dry-run `walk_preview` diagnostic | Root cause of the 58 % pollution; one shared predicate for full + incremental | **M** | **M** — new dep; over-exclusion risk → mitigated by dry-run preview before purge | `walk_preview` shows `target-dev/doc` excluded; new full index has 0 artifact chunks |
| **P2 — One-time purge** | `purge_excluded` (filter-only, no re-embed) + graph-clean + post-purge `build_semantic`; manifest-version migration trigger + explicit verb | Cleans existing live index immediately; delivers the routing-quality win | **S** | **L** — read-modify-rewrite of files we already own | artifact chunks ~58 % → ~0; concept set re-clusters clean |
| **P3 — Content-hash manifest** | `manifest.json` schema; hash-confirmed diff/apply; chunks-first/manifest-second atomicity; per-ws write-lock; model/dim guard | Robust incremental; kills mtime false-positives; rare full rebuilds; closes model-swap corruption | **M** | **M** — atomicity ordering + locking must be exact | touch-no-change ⇒ 0 re-embeds; crash-mid-pass ⇒ next pass converges; model swap ⇒ full rebuild |
| **P4 — Concept stability** | Greedy centroid-carry-over IDs + persisted ordinal allocator; `dangling` flag on relations + `sweep_dangling`; counts in build/relations replies | Authored relations survive recompute (Option A); orphans flagged not deleted | **M** | **M** — touches isolated routing crate + relations; serde-default = migration-free | persisted relation still resolves after a recompute that grows the corpus; a dropped theme ⇒ its relation flagged dangling, not deleted |

**Sequencing notes**
- P2 depends on P1 (`ExclusionMatcher`).
- P4's stable-id baseline is best established by P2's post-purge `build_semantic` — land P4
  before/with P2's re-cluster so the baseline is born stable. If P4 isn't ready, P2 can still
  purge chunks and defer the re-cluster.
- P3 is independent of P2/P4 but shares the matcher from P1.

---

## 7. How it's verified (concrete)

- **Exclusion (P1):** `walk_preview` on the live repo-root workspace reports the ~1 029+
  `target-dev/doc/*.html` paths as *excluded*; a fresh full index yields **0** chunks whose
  path contains `target-dev` / matches the deny-list. Unit tests: matcher excludes
  `<root>/Core/GUI/target-dev/doc/x.html`, `<root>/rust/target-dev/...`, honors a synthetic
  nested `.gitignore`, honors `.wyldeignore`, and `is_indexable_path` agrees with the walk.
- **Purge (P2):** before/after counts; `excluded_remaining == 0`; the ~58 % artifact share
  drops to ~0. Concept count/labels reflect real source dirs only.
- **Incremental (P3):** (a) `touch` an unchanged file ⇒ 0 re-embeds (hash equal); (b) edit a
  file ⇒ only its chunks re-embed, others byte-identical; (c) delete ⇒ its chunks + graph
  nodes gone; (d) simulated crash between chunks-write and manifest-write ⇒ next pass
  converges with no missing vectors; (e) change `WYLDE_EMBED_DIM` ⇒ full rebuild triggered.
- **Stability (P4):** author a relation on a `sem:` concept; grow the corpus + recompute ⇒
  the relation still resolves (id carried over). Force a theme to vanish ⇒ its relation is
  flagged `dangling` (count surfaced), still present on disk, excluded from routing.

---

## 8. Key decisions for Aaron

1. **Walker integration** — *(Recommended)* consult an `ignore::gitignore` matcher inside the
   existing hand walker + share it with `is_indexable_path`, rather than replacing the walker
   with `ignore::WalkBuilder`. Adds the `ignore` crate (~5 transitive: globset, walkdir,
   same-file, regex-automata; memchr already in-tree). OK to add the dep?
2. **Deny-list precedence** — *(Recommended)* built-in deny-list is a **hard backstop** that a
   `.wyldeignore !` re-include cannot override (except `.git/`, never indexed). Or: let
   `.wyldeignore !` win over the deny-list?
3. **`.html` suffix** — *(Recommended)* **do not** blanket-skip `.html`; rely on the
   `target-*` dir rule + gitignore so legit HTML workspaces still index. Agree?
4. **Hash strategy** — *(Recommended)* `sha256(file bytes)` with an `(mtime, size)` fast-path
   to avoid reading unchanged files; hash only when mtime/size differ. (Alt: hash chunk
   contents.) OK?
5. **Concept stable-ID** — *(Recommended)* greedy nearest-centroid carry-over (τ ≈ 0.85) +
   persisted, never-reused `sem:<n>` allocator; **orphaned relations flagged `dangling`,
   never auto-deleted**. Agree on τ and the flag-don't-delete rule?
6. **Purge rollout** — *(Recommended)* one-shot auto-migration keyed on manifest `version`
   (self-heals existing installs) **plus** an explicit `workspaces.reindex_purge` verb;
   **filter-only, no re-embed**. Agree?
7. **Model/dim-change guard** — *(Recommended)* store `embed_model`+`embed_dim` in the
   manifest; a mismatch forces a full rebuild. Agree?

---

## 9. Files this design will touch (for the build pass — none touched now)

- `rag/indexer/exclude.rs` *(new)* — `ExclusionMatcher`.
- `rag/indexer/walk.rs` — consult matcher in `walk_dir`; `is_indexable_path` delegates.
- `rag/indexer/manifest.rs` *(new)* — manifest schema + load/save + diff.
- `rag/indexer/mod.rs` — wire hash-diff into `reindex_delta`/`reindex_full`; model/dim guard;
  write-lock.
- `rag/indexer/store.rs` — manifest path helpers alongside chunks/state.
- `rag/indexer/delta.rs` — hash check in `upsert_file`; purge helper.
- `watcher/mod.rs` — cache/invalidate the matcher on the active watcher; `.wyldeignore`/
  `.gitignore` change → rebuild matcher.
- `concepts/semantic.rs` — stable-id carry-over (take prior concepts).
- `concepts/api.rs` — pass prior `Embedding` concepts into the matcher; `dangling_count` in
  build replies; purge verb.
- `concepts/relations_bridge.rs` — `sweep_dangling`; surface dangling in graph/list.
- `wylde-concept-routing` (relations crate) — `dangling` serde-default field on `Relation`.
- `action_dispatch.rs` / `api.rs` — register `workspaces.reindex_purge`,
  `workspaces.rag.walk_preview`.
- `Cargo.toml` — add `ignore`.

> Tier note for the new verbs (per spec §7 / Appendix A, the [[wylde-tbs-spec-over-brief]]
> rule): `walk_preview` = read-only diagnostic (Fast/idempotent/no-cache); `reindex_purge` =
> mutating, idempotent, NoRetry. Confirm against the spec table at build time.
