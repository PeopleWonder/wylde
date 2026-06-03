# Tool-Registry Verb Cutover (consolidation Slice 6 + 4b)

**Status:** Slice 6 landed 2026-06-03; **Slice 4b** (resource migration of
the last 11 named tools) landed 2026-06-03.
**Flag:** `WYLDE_HARNESS_VERB_TOOLS` — **default flipped from off to on.**
**Plan:** `docs/plans/tool-registry-consolidation.md` §6 (Slice 6) + §6
follow-up (Slice 4b), gitignored.

> **Slice 4b update (2026-06-03).** The "known gap" Slice 6 documented is
> now closed. The 11 "awaiting migration" tools (ollama×4, time×2, diff×1,
> voice transcribe/synthesize×4) are registered as the `model` / `time` /
> `diff` / `voice` resources and retired from advertising. The model-facing
> catalog dropped from **23 → 12** (8 verbs + 4 permanent imperative voice
> device tools). The four voice mic/wake-word tools are the only survivors.
> Details inline below.

> Filename note: the Slice-6 brief asked for this to live at
> `docs/wylde-phase5-cutover.md`, but that path already documents the
> unrelated **Phase-5 chat-turn driver** cutover. To avoid clobbering it,
> the verb-cutover record lives here (`wylde-phase6-cutover.md`) — the
> tool-registry consolidation is the Phase-6 tooling work.

---

## What changed

Before Slice 6 the model saw the full active tool catalog (~43 named
tools) and the eight verb tools side by side; `WYLDE_HARNESS_VERB_TOOLS`
defaulted **off**, so the verb path was a dark, opt-in shadow surface.

Slice 6 is the **cutover**:

1. **`WYLDE_HARNESS_VERB_TOOLS` now defaults to `on`.** The variable still
   exists as an opt-out escape hatch (set it to `0`/`false`/`no`/`off`).
   Taking the off-path now emits a one-shot **deprecation warning** — the
   legacy named-tool advertising surface is slated for removal.
   - Single source of truth: `wylde-harness::tooling::resource::verb_mode_active`.
   - Lockstep twin (separate crate): `wylde-extension-bridge::verb_mode_active`.
   - The two former harness copies were collapsed into the canonical one.

2. **The model-facing catalog is now the verbs + a small named tail.**
   `turn::prompt::{build_system_prompt, build_tools_field}` take a
   `verb_mode` bool. In verb mode they advertise only:
   - the **eight verb tools** (`wylde_describe / list / get / create /
     update / delete / search / execute`), and
   - the **surviving named tools** (`SURVIVING_NAMED_TOOLS` in
     `turn/prompt.rs`).

   Every other active named tool that has a **resource equivalent** is
   *retired from advertising*. Its handler stays registered and
   dispatchable (so a model that still emits `memory.search` works, and
   `tools.list` still lists it) — it is simply no longer shown in the
   prompt or the native `tools:` field. The cutover is **advertising-only**;
   no handler was deleted.

3. **Prompt guidance** in verb mode gains a discovery line ("call
   `wylde_describe` first to learn the `resource_type` values") and the
   one-sentence rule separating verbs from the named tail. The memory rule
   is rephrased in verb terms (`wylde_create("memory", …)` etc.).

`tools.list` (the IPC introspection endpoint) is intentionally left at the
**full** catalog — it is a management/discovery API, not the LLM surface.

---

## Catalog size — before vs after

Built from the full default registry (`Registry::default()`), counting the
native `tools:` field:

| Mode | Advertised tools (Slice 6) | After Slice 4b |
|---|---|---|
| Legacy (`verb_mode = false`) | **43** | 43 |
| Cutover (`verb_mode = true`)  | **23** | **12** |

Post-4b: 12 = 8 verbs + 4 surviving named tools (the imperative voice
device triggers only). Asserted exactly by
`tools::verbs::tests::cutover_catalog_is_exactly_verbs_plus_survivors`.

---

## Retired from advertising (31 named tools — resource-backed)

20 retired in Slice 6 + 11 more in Slice 4b. All still registered +
dispatchable; reachable through the verbs.

### Slice 4b additions (11)

| Retired named tool | Reached via |
|---|---|
| `ollama.list_loaded_models` | `wylde_list("model")` |
| `ollama.preload_model` | `wylde_create("model", {body:{model, keep_alive}})` |
| `ollama.evict_model` | `wylde_delete("model", <tag>)` |
| `ollama.auto_evict_lru` | `wylde_execute("model", "auto_evict_lru", …)` |
| `time.now` | `wylde_get("time")` |
| `time.format` | `wylde_execute("time", "format", {params:{epoch_ms, tz}})` |
| `diff.show_diff` | `wylde_execute("diff", "diff", {params:{a, b}\|{a_path, b_path}})` |
| `voice.transcribe` | `wylde_execute("voice", "transcribe", …)` |
| `voice.transcribe_stream` | `wylde_execute("voice", "transcribe", {params:{stream:true}})` |
| `voice.synthesize` | `wylde_execute("voice", "synthesize", …)` |
| `voice.synthesize_stream` | `wylde_execute("voice", "synthesize", {params:{stream:true}})` |

**Resource shapes adopted (4b):**
- **`model`** — CRUD + one execute. A *loaded model* has identity (the
  Ollama tag): preload→`create`, evict→`delete`, list_loaded→`list`. The
  `auto_evict_lru` sweep has no single-tag identity, so it stays an
  `execute` action. **Not** the Slice-3a `models.*` model-registry cluster
  — that surface (`crate::model_registry`) is routing/profile metadata and
  was never a verb resource. No duplicate registration.
- **`time`** — singleton `get` (current time) + `execute(action="format")`
  for arbitrary-epoch formatting. Both read-only.
- **`diff`** — single `execute(action="diff")`. Pure compute; read-only.
  The mutating `apply_patch` counterpart stays a deferred named tool.
- **`voice`** — `execute(action="transcribe"|"synthesize")` with a
  `params.stream` flag collapsing the 4 named tools into 2 actions. Pure
  inference; read-only. The mic/wake-word **device** tools are NOT part of
  this resource (permanent imperatives, below).

### Slice 6 originals (20)

| Retired named tool | Reached via |
|---|---|
| `memory.long_term.save` | `wylde_create("memory", …)` |
| `memory.update` | `wylde_update("memory", …)` |
| `memory.delete` | `wylde_delete("memory", …)` |
| `memory.search` | `wylde_search("memory", …)` |
| `rag.ask` | `wylde_search("rag_chunk", …)` |
| `rag.index` | `wylde_execute("rag", "index", …)` |
| `rag.reindex` | `wylde_execute("rag", "reindex", …)` |
| `rag.prune` | `wylde_delete("rag_chunk", {filter})` |
| `rag.feedback` | `wylde_create("rag_feedback", …)` |
| `rag.misses` | `wylde_list("rag_miss", …)` |
| `rag.chunk_usage` | `wylde_list("rag_chunk_usage", …)` |
| `rag.graph_stats` | `wylde_get("rag_graph_stats")` |
| `meta.graph_query` | `wylde_search("graph", …)` |
| `meta.tool_search` | `wylde_describe` |
| `fs.read_file` | `wylde_get("fs_file", path)` |
| `fs.list_files` | `wylde_list("fs_file"/"fs_dir", …)` |
| `fs.write_file` | `wylde_create("fs_file", …)` |
| `fs.edit_file` | `wylde_update("fs_file", …)` |
| `search.code_search` | `wylde_search("fs_file", …)` |
| `search.code_search_files` | `wylde_search("fs_dir", …)` |

---

## Surviving named tools (4 — post-4b)

### Imperative — permanent (4)

Stateful device-lifecycle triggers with no resource identity (plan §7).
Named by design; never collapse into a verb.

- `voice.mic.start`, `voice.mic.stop`
- `voice.wakeword.start`, `voice.wakeword.stop`

### Awaiting resource migration — temporary (0)

**Empty after Slice 4b.** The 11 tools that used to sit here (ollama×4,
time×2, diff×1, voice transcribe/synthesize×4) are now resource-backed and
retired — see "Slice 4b additions" above. No named tool is left waiting on
a resource.

---

## Known gap / follow-up — CLOSED by Slice 4b

> **Resolved 2026-06-03.** The gap below was the Slice-6 honest tail; Slice
> 4b registered the four missing resources (`model`, `time`, `diff`,
> `voice`) and retired all 11 tools. Kept here for the record.

The consolidation plan's Slice 4 scoped `fs/search/ollama/time/diff` but
~~**only `fs` + `search` shipped**~~ — Slice 4b shipped the rest. The 11
"awaiting migration" tools that ~~**cannot** be retired yet~~ are now
retired through their resources.

`execute_bash` / `execute_python` are not in either list: they remain
**deferred** (no Rust sandbox decision yet), so they are not advertised in
either mode. When they port, they join the *permanent imperative* tail
(arbitrary code execution has no resource identity).

---

## Rollback

Set `WYLDE_HARNESS_VERB_TOOLS=0` (logs a deprecation warning). The legacy
named-tool catalog returns; the verb tools remain present (they are
ordinary catalog entries), they just stop being the *only* advertised
surface.
