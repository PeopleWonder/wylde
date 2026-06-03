# Tool-Registry Verb Cutover (consolidation Slice 6)

**Status:** landed 2026-06-03.
**Flag:** `WYLDE_HARNESS_VERB_TOOLS` — **default flipped from off to on.**
**Plan:** `docs/plans/tool-registry-consolidation.md` §6 (Slice 6),
gitignored.

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

| Mode | Advertised tools |
|---|---|
| Legacy (`verb_mode = false`) | **43** |
| Cutover (`verb_mode = true`)  | **23** |

23 = 8 verbs + 15 surviving named tools. Asserted exactly by
`tools::verbs::tests::cutover_catalog_is_exactly_verbs_plus_survivors`.

---

## Retired from advertising (20 named tools — resource-backed)

All still registered + dispatchable; reachable through the verbs.

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

## Surviving named tools (15)

### Imperative — permanent (4)

Stateful device-lifecycle triggers with no resource identity (plan §7).
Named by design; never collapse into a verb.

- `voice.mic.start`, `voice.mic.stop`
- `voice.wakeword.start`, `voice.wakeword.stop`

### Awaiting resource migration — temporary (11)

Execute/CRUD-shaped tools whose **resource cluster was never registered**
in Slices 1–5a. Retiring them now would orphan the operation (the verb
path has nowhere to dispatch), so they stay named until a follow-up slice
registers their `ResourceDefinition`.

- `ollama.list_loaded_models`, `ollama.preload_model`,
  `ollama.evict_model`, `ollama.auto_evict_lru` → future `model` resource
- `time.now`, `time.format` → future `time` resource
- `diff.show_diff` → future `diff` resource
- `voice.transcribe`, `voice.synthesize`, `voice.transcribe_stream`,
  `voice.synthesize_stream` → future voice `execute` resource

---

## Known gap / follow-up (the cutover's honest tail)

The consolidation plan's Slice 4 scoped `fs/search/ollama/time/diff` but
**only `fs` + `search` shipped** (`fs_file`/`fs_dir`); the `ollama`/`time`/
`diff` portion was deferred to a follow-up, and a voice `execute` resource
was never sliced. As a result, the 11 "awaiting migration" tools above
**cannot** be retired yet without orphaning them — per the cutover's hard
rule, a tool with no resource equivalent stays advertised, not silently
dropped.

**Follow-up slice (Slice 4b / voice-execute):** register `model`, `time`,
`diff`, and a voice `execute` resource, then move those 11 tools into the
retired set. That shrinks the model-facing catalog from 23 to ~12.

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
