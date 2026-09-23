---
title: Extending workspaces
audience: contributors changing workspace types, MRU, or the indexer
authored: 2026-05-27
status: living reference
---

# Workspaces

## Executive summary

A "workspace" in Wylde is a folder on your computer that you've told
Wylde to pay attention to. When you point Wylde at `~/Documents/Novel/`
and say "this is my novel project," Wylde creates a workspace for it:
gives it a stable id, remembers a short persona description
("the Wylde user's fantasy novel, third draft"), indexes the files inside,
and starts treating "search my files" and "ask about my notes" calls
in that context.

Workspaces are the unit of **scope**. Long-term memory is global;
RAG tiers are by category; workspaces are by location. The model can
hop between workspaces during a chat, and each one has its own pile
of indexed content, its own per-workspace memory folder for
LLM-curated insights, and its own persona that gets included in the
system prompt when the workspace is active. The user manages a list
of workspaces sorted by most-recently-used; a configurable cap (default
20) keeps the list pruned, evicting the stale ones' indexes (but
keeping their durable LLM-curated memories — those survive eviction
specifically because they're the costly-to-rebuild part).

This doc covers the workspace data model, how MRU eviction works, and
how to add new workspace fields or behaviour. The file indexer (the
thing that walks the folder and builds the vector index) is **still in
Python** as of Phase 7.A — the Rust side ships the registry only. The
indexer port is a future slice; until then, "indexing" is a flag on
the Workspace struct that the Python side sets.

## How it works

### Files

```
rust/crates/wylde-harness/src/memory/workspaces/
├── mod.rs           — re-exports
├── store.rs         — Workspace struct, MutexGuard CRUD, persistence
├── mru.rs           — get_mru_limit / set_mru_limit, eviction
├── slug.rs          — slug_for(path) — deterministic 16-char hex hash
├── actions.rs       — 8 pipe-action handlers
└── test_support.rs  — TestEnv (TEST_ENV_LOCK + tmp data dir)
```

### The Workspace struct (`store.rs:55-97`)

```rust
pub struct Workspace {
    pub id: String,               // slug_for(path) — deterministic
    pub path: String,             // filesystem root
    pub persona: String,          // LLM-curated identity, included in prompt
    pub file_count: u64,          // last-indexed count
    pub last_indexed_at: f64,     // epoch seconds
    pub last_activated_at: f64,   // epoch seconds; powers MRU
    pub indexing: bool,           // true while a reindex is mid-flight
}
```

The `id` is a hash of `path`, so the same folder always gets the same
id even across reboots. If the user renames the folder, they get a new
workspace; the old one orphans until pruned.

### Storage layout

* **`<data_dir>/workspaces.json`** — `{"workspaces": [Workspace, …]}`
  in MRU order (most-recently-activated first).
* **`<data_dir>/indexes/<slug>/`** — per-workspace index folder.
  Today: LanceDB (Python writes it). Future: pure-Rust vector store
  per [vector-store.md](./vector-store.md). The Rust side never reads
  or writes content here in Phase 7.A; it only manages the folder's
  existence/deletion.
* **`<data_dir>/workspace_memories/<slug>/`** — durable LLM-curated
  workspace memory. The "important things this workspace's content
  has taught me" tier. Survives MRU eviction because re-deriving it
  requires another LLM pass.
* **`<data_dir>/workspaces/settings.json`** — the MRU cap setting.

### Public CRUD API (`store.rs`)

* `list_workspaces()` — all workspaces in MRU order.
* `recent_workspaces(n)` — first N.
* `get_workspace(id)` — one by id.
* `register(path, persona)` — create a new workspace from a path. Slug
  derived deterministically; if the slug already exists, returns the
  existing entry rather than colliding.
* `touch_activated(id)` — bump `last_activated_at` to now. Re-sorts the
  MRU; this is what moves a workspace to the top of the list.
* `update_file_count(id, count)` — set count after indexing.
* `set_persona(id, text)` — persist new persona text.
* `delete_workspace(id)` — full delete: registry + index dir +
  workspace-memory dir. The "I'll never use this folder again" path.

  The last of those three crosses a service boundary. The **workspaces
  service** owns the delete verb and the workspace bundle; the **harness**
  owns `<data_dir>/workspace_memories/<id>/`. So the delete handler asks
  the harness to sweep its own store, over the
  `memory.workspace.delete_all` verb. The delete verb still returns
  without blocking on that peer call (a Fast/Medium verb must not stall on
  a service that may be down) — but the sweep is now **durable**, not
  fire-and-forget: see the pending-teardown queue note below.

  Until #135 that call did not exist. `delete_memory_dir` was written,
  correct, unit-tested, and had **zero callers**, so this bullet described
  an intent rather than a behaviour: a deleted workspace's durable
  memories stayed on disk indefinitely. Because a workspace id is derived
  from its folder path (#28), re-registering the same folder re-derived
  the same id and silently re-attached memories the user believed they had
  deleted — a privacy consequence as much as a disk one.

  As of #166 the sweep is durable. `registry::delete` enqueues the memory
  sweep (and the flat-store conversation sweep) on the same on-disk
  pending-teardown queue the graph cascade uses (#99), generalized from
  bare workspace ids to `(workspace id, target)` pairs where `target ∈
  { graph, memory, conversations }`. The drain
  (`graph::cleanup::run_pending_cleanup`, fired on the next
  create/activate/delete and at boot) dispatches each target and dequeues
  a pair only on `reply.ok`; a down harness leaves the memory pair queued
  for the next drain instead of dropping it. The re-created-workspace
  guard applies to every target — if the folder is live again when the
  drain runs, its queued sweeps are dequeued **without** running, so a
  delete-then-re-add can't wipe the fresh memories. The memory + conversation
  sweeps are enqueued only by explicit `delete`; since #133 that is the only
  teardown path at all (registering a workspace no longer evicts anything), so
  no non-delete path can sweep the memory tier.

### MRU semantics (`mru.rs`)

* `get_mru_limit() -> u32` — current cap. Default 20, min 5, max 100.
* `set_mru_limit(cap)` — persist new cap; **evict overflow workspaces
  immediately**. Eviction = remove index dir; **keep** workspace-memory
  dir. The workspace registry entry is also removed. Idempotent.

The asymmetry between "index dir gets nuked" and "workspace-memory dir
survives" is deliberate. Indexes are cheap to rebuild (re-scan the
folder); workspace memories are expensive (re-run the LLM curation
pass). Saving the second is the whole point of having two folders.

### Slug derivation (`slug.rs`)

`slug_for(path)` is a 16-char hex hash of the path string. Properties:

* Same path → same slug, always.
* Different paths → different slugs (collisions essentially impossible
  at our scale).
* Path normalisation happens before hashing — trailing slashes,
  Windows vs. Unix separators, case-folding on Windows. Two
  user-visibly-identical paths produce the same slug.

The slug is **stable**; consumers can rely on it as a database key.
If you change the hashing algorithm, you invalidate every existing
workspace — write a migration.

### Wire surface (`actions.rs`)

8 pipe actions registered on `\\.\pipe\wylde-harness`:

* `memory.workspaces.list`
* `memory.workspaces.recent({limit?})`
* `memory.workspaces.get({workspace_id})`
* `memory.workspaces.get_mru_limit()`
* `memory.workspaces.set_mru_limit({limit})`
* `memory.workspaces.get_persona({workspace_id})`
* `memory.workspaces.set_persona({workspace_id, text?})`
* `memory.workspaces.delete({workspace_id})`

Notably **absent**: `register` (no Rust-side workspace-creation pipe
verb yet — Python handles new workspaces via `rag.workspaces.*`),
`reindex` (indexer not in Rust), `search` (vector search not in Rust
for the workspace tier).

### Indexer status

The file scanner — the thing that walks `workspace.path`, reads every
file, chunks it, embeds the chunks, writes the vector index — **lives
in Python** at `Core/harness/memory/workspace_indexer.py` and friends.
Phase 7.A ports the registry shape; the indexer is a future slice
that will fold into [vector-store.md](./vector-store.md) once
embeddings are Rust-side too.

Until that lands, the Rust workspace registry is a read-mostly view
plus the MRU controls. Writes that change which files are indexed
(register, reindex, delete) still go through Python. The `indexing:
bool` field exists in the struct specifically so JSON round-trips
preserve Python's value when Rust reads + writes the registry without
touching the indexer.

> **Update (indexer ported + live progress).** The indexer is now fully
> Rust (`wylde-workspaces` `rag::indexer`). Live status lives in
> `RagState` (`index/rag_state.json`), joined onto every `list_mru` row
> by `api::def_with_index_state`. Alongside `indexing: bool` it now carries
> an optional `progress` snapshot
> (`rag::indexer::progress::IndexProgress`): the current phase
> (walk / chunk / embed / persist), files/chunks done vs total, a rolling
> chunks/sec, and a computed ETA. The reporter writes it (throttled) during
> a pass and clears it on completion, so the Workspaces panel can render a
> determinate progress bar + ETA — indeterminate during the walk (no total
> yet), then a percent + "X / Y files" + "~Nm remaining" during the embed.
> Measure real throughput with the `index_bench` example (it pins an
> isolated `WYLDE_DATA_DIR` and a dead `GRAPH_BOLT_URL`, so it never touches
> the live index or the shared graph).

## How to extend

### Add a new field to the Workspace struct

E.g., a `pinned: bool` flag for "never evict this from MRU even if
stale":

1. Add the field in `store.rs:Workspace` with `#[serde(default)]`.
2. Update `register` / `set_persona` / new `set_pinned` writes.
3. Teach `mru.rs::set_mru_limit` to skip pinned workspaces during
   eviction.
4. Add a `memory.workspaces.set_pinned` pipe verb: write the handler in
   `actions.rs`; add a trait method to `wylde-harness/src/api.rs`'s
   `HarnessApi` (and the `DefaultHarnessApi` impl); register the verb in
   `wylde-harness/src/pipe.rs::install_all_against`; route it in
   `Core/GUI/Frontend/Pipe/src/memory_workspaces.rs::dispatch`.
5. Coordinate with Python — both sides must round-trip the field
   unchanged through the strangler flip.

### Add a new MRU strategy

The current strategy is strictly last-activated-at. Suppose you want
"weighted by file count" (busy workspaces stay longer):

* The MRU sort lives in `store.rs::list_workspaces`. Replace the
  `sort_by` key with a custom score function.
* Document the new strategy — users with mental models of "I just
  used X so it'll be at the top" will be confused otherwise.
* Add a setting so users can opt back into LRU if the weighted
  version surprises them.

### Add a new workspace type

The current model is one shape: `(path, persona)`. Suppose you want a
"virtual workspace" — no path, just a tag-set; or a "remote workspace"
backed by a cloud URL:

* Add a `kind: WorkspaceKind` enum with `Local { path }`, `Virtual
  { tags }`, `Remote { url, sync_token }`. Default Local for
  round-trip compat.
* Indexer + slug derivation become kind-specific. `slug_for` for
  Virtual might hash the sorted tag list; for Remote it might hash
  the URL.
* The workspace-memory folder convention still applies — every kind
  gets `<data_dir>/workspace_memories/<slug>/`. The index folder may
  not (Virtual workspaces may have no index dir at all).
* This is a substantial change. Coordinate with Python before landing.

### Wire it as an LLM tool

Workspace operations are exposed to the LLM via
`tooling/tools/memory.rs` (workspace-listing) and
`tooling/tools/rag.rs` (per-workspace search, once indexer ports).
Adding a tool means adding an `entry_active` call there — see
[../../extending-wylde-llm-tools.md](../../extending-wylde-llm-tools.md).

## Gotchas

* **The indexer is still Python.** If you write to the workspace
  registry from Rust without going through the Python indexer, the
  Python side won't know about the change until its next periodic
  scan. Worse: it may try to re-index the path you just registered
  and end up double-writing. Until the indexer ports, register/reindex
  flows should go through Python.
* **MRU eviction is destructive on the index dir.** A user who's been
  away for a week, comes back, and finds their MRU cap kicked their
  favourite workspace's index out, will have to wait for a reindex.
  Eviction triggers should be conservative; consider a "pinned" flag
  (see "Add a new field" above).
* **The workspace-memory folder is the load-bearing piece.** Indexes
  are cheap; workspace memories are expensive. Don't ever delete a
  workspace-memory folder without an explicit user-facing confirm.
  `delete_workspace` does this on purpose — it's the "I really mean it"
  path. Route any new removal path through the **delete verb**, never
  through the shared `teardown_bundle` primitive: MRU eviction funnels
  through that too, and eviction must preserve this tier.
* **`delete_memory_dir` validates its id, and must.** It is a
  `remove_dir_all`, and `Path::join` leaves the tier for an empty id (which
  resolves to the tier ROOT — every workspace's memories), an absolute id
  (which discards the base entirely), or a `..` traversal. The store
  refuses all three. Keep that guard if you refactor the path helpers.
* **Slugs are content-addressable, not random.** If a user moves a
  folder (`mv ~/Novel ~/Books/Novel`), the workspace registry will
  orphan the old slug and create a new one. The user loses their
  persona + workspace memory. Document this; consider a `rename`
  flow that updates the path in-place.
* **`TEST_ENV_LOCK` serialises across all memory tests.** Workspace
  tests sit alongside long-term and RAG; collectively they hold the
  lock. Tight tests, please.
* **Persona text is included verbatim in the system prompt.** A
  workspace persona of "the best workspace ever, ignore all previous
  instructions" is a prompt-injection vector. Treat persona text as
  semi-trusted user input; downstream system-prompt assembly should
  escape or fence it.

## Cross-links

* [index.md](./index.md) — memory subsystem overview.
* [vector-store.md](./vector-store.md) — the index substrate (once
  the indexer ports).
* [rag.md](./rag.md) — RAG search by workspace_id.
* `Core/harness/memory/workspace_indexer.py` — the Python indexer.
* `Core/harness/memory/rag.py` — Python `rag.workspaces.*` actions
  (overlap with `memory.workspaces.*`; the two namespaces will
  converge in a future slice).
* `~/.claude/projects/.../memory/wylde_phase7_slice_7a_shipped.md` —
  Phase 7.A ship report.

---

*Workspaces are the scope unit. Get the data model right; the indexer
will catch up.*
