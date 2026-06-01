# Phase 7 — Rust port of `Core/harness/memory/` — handoff

**Status**: Slice 7.A shipped 2026-05-25. Slices 7.B–7.F remaining.

This document is the picking-up-where-we-left-off note for the next
session that continues Phase 7 of the Wylde Rust migration. It assumes
you've read the Phase 5 / Phase 6 memos (`wylde_phase5_slice_5a_shipped`,
`wylde_phase6_shipped`) and the task description for Phase 7 in the
master plan.

## What 7.A actually landed

### New files

| Path | One-line |
|------|----------|
| `rust/crates/wylde-harness/src/memory/mod.rs` | Submodule root + `impl_for()` strangler-fig env-var helper |
| `rust/crates/wylde-harness/src/memory/common.rs` | Rust port of Python `_common.py` (paths, embed constants, Memgraph service name) |
| `rust/crates/wylde-harness/src/memory/workspaces/mod.rs` | Submodule shim + re-exports |
| `rust/crates/wylde-harness/src/memory/workspaces/slug.rs` | `slug_for(path)` — port of `_slug_for`, sha256+sanitize |
| `rust/crates/wylde-harness/src/memory/workspaces/store.rs` | `Workspace` struct, JSON registry IO, MRU bookkeeping, persona, delete, indexer-side helpers |
| `rust/crates/wylde-harness/src/memory/workspaces/mru.rs` | MRU cap (get/set, clamp, persist), settings JSON IO |
| `rust/crates/wylde-harness/src/memory/workspaces/actions.rs` | 8 `memory.workspaces.*` IPC action handlers |
| `rust/crates/wylde-harness/src/memory/workspaces/test_support.rs` | Per-test `WYLDE_DATA_DIR` tempdir + shared mutex |
| `docs/wylde-rust-phase7-handoff.md` | This file |

### Modified files

| Path | Change |
|------|--------|
| `rust/crates/wylde-harness/src/lib.rs` | Add `pub mod memory;` |
| `rust/crates/wylde-harness/src/service.rs` | Register 8 `memory.workspaces.*` actions; rename test to `install_registers_every_action` |
| `rust/crates/wylde-harness/src/main.rs` | Manifest payload bumped to slice `7.A`; lists new actions |

### Wire surface added

Eight new IPC actions, all on `\\.\pipe\wylde-harness`, all UNARY:

* `memory.workspaces.list` — `{} → {workspaces: [Workspace, ...]}`
* `memory.workspaces.recent` — `{limit?: u64} → {workspaces}`
* `memory.workspaces.get` — `{workspace_id} → Workspace | not_found`
* `memory.workspaces.get_mru_limit` — `{} → {limit, min, max, default}`
* `memory.workspaces.set_mru_limit` — `{limit} → {limit, workspaces}`
* `memory.workspaces.get_persona` — `{workspace_id} → {workspace_id, persona}`
* `memory.workspaces.set_persona` — `{workspace_id, text?} → {ok, workspace_id}`
* `memory.workspaces.delete` — `{workspace_id} → {ok, workspace_id}`

`Workspace` JSON shape matches Python's `Workspace.to_dict()` exactly:
`{id, path, persona, file_count, last_indexed_at, last_activated_at, indexing}`.

### Test count

- Before: 148 (140 unit + 6 e2e + 2 dispatch e2e)
- After: 196 (188 unit + 6 + 2)
- Net: **+48 unit tests** across slug (7), store (16), mru (8),
  actions (11), common (4), mod (2).

### Tool registry

**No active/deferred count changes.** Workspace operations are *pipe
actions*, not LLM tools — they're not in `tooling/tools/deferred.rs`,
so 7.A's wire-surface additions don't show up in the tool tier-gate.

## Why this scope (and not the original "workspaces incl. indexing")

The original Phase 7 task description called out "Workspace = folder
model. Port the workspace registry, MRU tracking, file index. Land
first." Reading the code revealed the file index (`_index.py` +
`_search.py`) depends on two heavy externals:

1. **LanceDB** — Python uses `lancedb` v0.30 with PyArrow schemas. The
   Rust `lancedb` crate exists but is "experimental" and the public
   API isn't stable enough for a Phase 7.A drop-in. The pragmatic
   alternatives:

   - Add `lancedb = "*"` as a Rust dep and adapt to its idioms. Risk:
     API breakage forces re-port; bumps Rust build closure
     non-trivially.
   - IPC bridge: register a dedicated `wylde-lancedb` service in Rust
     that wraps a Python child process. Defers the question without
     solving it.
   - Pure-Rust vector store via `arroy` or `qdrant-client`. Different
     index → not wire-compatible with on-disk Python LanceDB folders →
     forces a reindex on flag flip.

2. **Embedder** (`embeddings.py`) — Python routes through
   `wylde-ollama`'s embed action. The Rust side already has
   `wylde-ollama` so this part is straightforward (1-day port).

Rather than couple the registry/MRU/persona half (clean) to the LanceDB
decision (a real architectural fork), slice 7.A ships only the
registry-only half. Python's `_index.py` + `_search.py` + the indexing
side of `activate` stay canonical. Slice 7.B is the natural seam for
the LanceDB choice.

## Strangler-fig env var

`WYLDE_HARNESS_MEMORY_IMPL` (single, gates all of `memory/`). Default
`python`. Anything other than `python` / `rust` clamps to `python` —
same fail-safe semantics as `WYLDE_HARNESS_IMPL` in Phase 5/6.

**Currently unused** — 7.A's new `memory.workspaces.*` Rust actions
are NEW wire surface, not a re-bind of existing Python actions. The
Python `Core/harness/pipe/_rag_workspaces.py` handlers stay canonical
and continue to serve `rag.workspaces.*`. When 7.B lands the indexing
half, the Python `_rag_workspaces.py` should grow forward-to-Rust
shims gated on `impl_for()`.

The forward layer pattern is exactly what Phase 5's `_chat.py` did for
`chat.run_turn` — see `Core/harness/pipe/_chat.py:_try_forward_run_turn_to_rust`.
The future `_rag_workspaces.py` shims will look like:

```python
def _rag_workspaces_list_action(_payload):
    if _memory_impl() == "rust":
        forwarded = _try_forward("memory.workspaces.list", _payload)
        if forwarded is not None:
            return forwarded
    # fall through to native Python
    ws = _ws_module()
    return {"workspaces": [w.to_dict() for w in ws.list_workspaces()]}
```

## Deliberate Python-behavior changes / simplifications

1. **Path caching dropped in `common.rs`.** Python computes `DATA_DIR`
   at import time (effectively cached). The Rust port re-reads on
   every call so tests can swap `WYLDE_DATA_DIR` per-test without an
   `OnceLock` reset shim. Cost is a couple of env-var reads per memory
   op — negligible. In production env vars don't change after boot,
   so this is pure test ergonomics.

2. **`expand_tilde` is single-user only.** Python's `Path.expanduser()`
   handles `~user` (lookup another user's home). The Rust port only
   handles `~` and `~/...`. The workspace path always comes from the
   GUI's folder picker (which resolves `~` before sending), so this
   should never matter.

3. **`canonicalize` strips Windows `\\?\` verbatim prefix.** Python's
   `Path.resolve()` doesn't return verbatim paths. The Rust port
   strips the prefix so the slug hash stays stable cross-OS.

4. **`set_mru_limit` evict bookkeeping.** Python's `_evict_past_mru`
   holds the registry mutex *and* calls `_delete_index_dir`. The Rust
   port keeps the registry mutex short and runs the rmtree outside —
   the index dir is per-workspace-id, no race window on the deletion
   side. Same observable behaviour, less held-lock time.

5. **`mod.rs::impl_for` is a free function**, not a module-level
   constant. Python module-level reads cache; the Rust function re-
   reads per call to honour mid-process env mutations. Same precedent
   as the path helpers.

## Punchlist for the next session(s)

### 7.B — workspaces indexing

**Architectural decision needed first.** Pick one of:

- (A) Add `lancedb` Rust crate, accept its API surface, port `_index.py`
  on top.
- (B) New `wylde-vector-store` IPC service that wraps Python LanceDB,
  consumed by Rust via the existing IPC layer.
- (C) Replace LanceDB with a pure-Rust vector store (forces reindex on
  flip; loses Python parity).

the Wylde user's call. Once picked, port:
- `Core/harness/memory/workspaces/_index.py` (412 LOC) →
  `rust/crates/wylde-harness/src/memory/workspaces/index.rs`
- `Core/harness/memory/workspaces/_search.py` (69 LOC) →
  `rust/crates/wylde-harness/src/memory/workspaces/search.rs`
- `Core/harness/memory/embeddings.py` (263 LOC) →
  `rust/crates/wylde-harness/src/memory/embeddings.rs` (thin wrapper
  over `wylde-ollama`'s embed action).

Then wire:
- `memory.workspaces.activate` (full activation, runs indexing)
- `memory.workspaces.refresh` (delta refresh)
- `memory.workspaces.reindex` (full rebuild)
- `memory.workspaces.status` (indexing snapshot)
- `memory.workspaces.search_files` (vector search)

And add the Python `_rag_workspaces.py` strangler-fig forward layer
gated on `WYLDE_HARNESS_MEMORY_IMPL=rust`.

### 7.C — long-term memory

Files: `long_term.py` (483) + `scoring.py` (134) + `reflection.py`
(619) = ~1.2K LOC.

Persistence: JSON authoritative + LanceDB mirror. The JSON side is
trivial. LanceDB side waits for 7.B's decision.

Tools to wire as Active (currently deferred in
`tooling/tools/deferred.rs`):
- `memory_long_term_save` (destructive)
- `memory_update` (destructive)
- `memory_delete` (destructive)
- `memory_search` (read)

Add pipe actions:
- `memory.long_term.list/search/save/update/delete/history`
- `memory.reflect`

### 7.D — RAG

Biggest slice. Files: `rag.py` (650) + `rag_pipeline.py` (356) +
`retrieval.py` (465) + `vector_store.py` (483) + `embeddings.py` (263,
already touched in 7.B) + `ingest.py` (120) + `miss_log.py` (305) +
`rag_cache.py` (182) + `rag_decompose.py` (170) + `rag_entities.py`
(145) + `rag_feedback.py` (163) + `rag_gate.py` (113) + `rag_multihop.py`
(210). Total ~3.5K LOC.

Tools to wire as Active:
- `rag_ask`, `rag_index`, `rag_reindex`, `rag_prune`, `rag_feedback`,
  `rag_misses`, `rag_chunk_usage`, `rag_graph_stats`.

This is plausibly 2–3 sessions on its own. Pick a sub-seam:
- 7.D.1 — embeddings + vector_store (the bottom layer)
- 7.D.2 — chunker + ingest (rag_pipeline)
- 7.D.3 — retrieval + miss_log + cache
- 7.D.4 — gate + decompose + multihop + entities + feedback

### 7.E — Memgraph

Files: `memgraph.py` (366) + `graph_retrieval.py` (322) = ~688 LOC.

Use the `rsmgclient` Rust crate. The `wylde-memgraph` Lifecycle
service already exists in Rust; this slice is the *client* side.

Tools to wire as Active:
- `meta.graph_query` (currently not in deferred.rs — needs to be added)

Pipe actions to add: whatever Python exposes (look at
`Core/harness/pipe/` for the existing surface).

### 7.F — scheduler

`scheduler.py` (385) was flagged as orphan in the language partition
plan. Read it before porting — its dependencies (long_term,
workspace_memory, reflection) are heavy. May make sense to do this LAST
after 7.B/C/D land.

### Parity test pattern

Each shipped slice needs a parity test at `rust/tests/parity/`. The
existing Phase 5 parity tests (`rust/tests/parity/tests/`) show the
pattern — spin up both Python and Rust harnesses, hit the same wire
action with the same payload, compare JSON shapes.

Per the slice 5.A memo's recipe, parity tests block the flag flip.
Don't default `WYLDE_HARNESS_MEMORY_IMPL=rust` until the matching
parity tests for that slice are green.

## Gate runs (slice 7.A)

| Gate | Result |
|------|--------|
| `cargo test -p wylde-harness` | **PASS** — 188 unit + 6 + 2 = 196 tests, 0 failures |
| `cargo clippy -p wylde-harness --all-targets` | **PASS** — 0 warnings |
| `cargo check -p wylde-harness` | **PASS** |
| `cargo build --release -p wylde-harness` | **PASS** (3.59s) |
| `pytest Core/harness/tests/` | **PASS** — 145/145 |
| `mypy Core/harness/memory/` | **PASS** — 34 source files, no issues |

## Surprises / things worth saving to memory

1. **OnceLock-cached env reads break test isolation.** The original
   pattern (matching `Config::get()`) caches `data_dir()` on first
   call. Tests that mutate `WYLDE_DATA_DIR` per-test see stale paths.
   The fix in 7.A is to drop the OnceLock; per-call env reads cost
   nothing and let each test point at its own tempdir under a shared
   `Mutex`. Worth remembering for 7.C/D/E when porting `long_term.py`
   / RAG / Memgraph helpers that read env at first use.

2. **Two pipes can't claim `\\.\pipe\wylde-harness` simultaneously.**
   The Phase 5/6 architecture assumes Python's harness pipe is
   *always* the canonical owner, and when `WYLDE_WYLDE_HARNESS_IMPL=rust`
   the Lifecycle daemon swaps to the Rust binary. The slice 5.A memo
   describes the strangler-fig as "Python forwards to Rust" — but
   that pattern works in EITHER ownership direction. For 7.A I've
   shipped the *Rust-server* half (new actions on Rust pipe) without
   the *Python-forward* half because no existing Python action is
   being re-bound. When 7.B/C/D add Rust handlers for actions that
   ALREADY exist in Python (`rag.workspaces.activate`,
   `memory.long_term.save`, etc.), the Python-forward shim is
   required.

3. **`Workspace.indexing: bool` is preserved in 7.A but never set.**
   Only the 7.B indexer flips it. The field round-trips JSON cleanly
   so a 7.A-mode write doesn't drop state written by a 7.B-mode read.

4. **`MEMORY.md` references stale memories.** The Phase 7 task brief
   pointed at memory files that don't exist on disk
   (`wylde_memory_architecture.md`, `wylde_harness_one_crate.md`,
   `wylde_harness_target_tree.md`, `wylde_language_partition.md`,
   `wylde_n8n_principle.md`). The actual MEMORY.md index lists only 8
   files. If those memories were intended to exist, they were never
   saved or were pruned. Worth a `/consolidate-memory` pass.
