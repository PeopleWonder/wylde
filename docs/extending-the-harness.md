---
title: Extending the harness
audience: contributors changing chat-brain internals; new sessions getting their bearings
authored: 2026-05-27
status: living reference
---

# Extending the harness

## Executive summary

The "harness" is the part of Wylde that does the thinking — when you
talk to Wylde, it's the harness that runs the chat, decides which tools
the model should use, calls those tools, remembers what was said,
remembers what's important to you, and keeps track of which language
models are available. It's one logical thing — one Rust crate called
`wylde-harness`, one binary, one named pipe — with five distinct
internal areas that handle different concerns. This doc is the map of
those five areas.

Extending the harness isn't the same as extending Wylde-as-a-whole. If
you want to add a tool the model can call, or a button on the GUI, or a
plugin — those have their own docs. This doc is for when you're
changing how the chat brain *itself* works: the way it parses the
model's responses, the way it scores memories for importance, the way
it picks which model to use for a request, the way it streams events
back to callers. The internal anatomy.

The five areas are: **turn** (the loop that drives one chat
exchange), **tooling** (the registry of LLM-callable functions),
**memory** (everything the system remembers — workspaces, long-term
facts, the vector store, the knowledge graph, RAG), **model_registry**
(the catalogue of every model the harness can route to), and **pipe**
(the IPC dispatcher that exposes harness verbs to the rest of Wylde).
This doc gives you the overview; each area has its own deep-dive doc
linked from below.

## How it works

### The five submodules

```
wylde-harness/src/
├── turn/              ← chat-turn driver, salvage parser, tool loop
├── tooling/           ← in-process tool registry + runner
├── memory/            ← workspaces, long_term, vector, memgraph, rag
├── model_registry/    ← model identity, heuristics, routing
├── pipe/              ← IPC dispatcher (also mirrored GUI-side; see below)
├── events.rs          ← wire types (TurnStarted, Chunk, ToolCall, …)
├── state.rs           ← per-turn TurnState + turn id generator
├── service.rs         ← install() / stop() / reset_for_tests()
├── dispatch.rs        ← internal vs MCP-extension routing
├── config.rs          ← env-driven Config
├── main.rs            ← binary entry: manifest, heartbeat, serve loop
└── lib.rs             ← public re-exports
```

Each submodule is a coherent concern. The cross-cutting helpers
(`config`, `state`, `events`, `dispatch`, `service`) glue them together;
`main.rs` is the binary entry that wires everything up.

### `turn/` — the chat-turn driver

Files: `mod.rs`, `actions.rs`, `salvage.rs`, `tool_round.rs`.

The turn driver is the engine that runs one chat exchange. The user
(or the GUI) calls `chat.run_turn`; the driver builds the request, sends
it to wylde-ollama, reads the streamed response, extracts any tool
calls the model emitted, dispatches them, feeds results back to the
model, and loops until the model produces a final answer or hits the
loop cap (`MAX_TOOL_LOOPS = 8`).

Extension points:

* **Salvage parser shapes** (`salvage.rs:60–75`) — three patterns recognise
  tool calls in the model output: fenced JSON, `<tool_call>…</tool_call>`
  tags, bare balanced-brace JSON. Adding a new shape (XML, YAML, JSONC)
  means adding a regex and updating the detection priority.
* **Tier gate** (`tool_round.rs:80–120`) — `check_tier_gate` consults the
  registry's `destructive` flag against the turn's device tier
  (`read_only`, `tool_use`, `destructive_tool_access`). Adding a new
  tier (e.g. `read_only_dangerous`) means updating the normalisation
  match.
* **Streaming event types** (`events.rs`) — `TurnStarted`, `Chunk`,
  `ToolCall`, `ToolResult`, `TurnEnded`, `ToolErrorReason`. Adding a new
  event variant (e.g. `ModelSwapped` for mid-turn model swaps) means
  updating the enum and every consumer.

Most "extending the turn driver" requests are actually requests to add
a new tool — see [extending-wylde-llm-tools.md](./extending-wylde-llm-tools.md).
Touch the salvage parser only when the model is emitting tool calls in
a shape the current parser can't see.

### `tooling/` — the in-process tool registry

Files: `registry.rs`, `runner.rs`, `tools/{fs,diff,search,meta,time_tools,memory,rag,ollama,voice,deferred}.rs`.

The tool registry is the canonical catalogue of LLM-callable functions.
Each tool has an id, a dotted name, a group, a description, a JSON-Schema
parameter list, a `destructive: bool` flag, and a handler (or a
deferred-stub tag). The runner takes a tool call from the model, looks
it up in the registry, applies the tier gate, and dispatches.

This is the most common extension point — see
[extending-wylde-llm-tools.md](./extending-wylde-llm-tools.md) for the
recipe. There's nothing in `tooling/` itself you need to extend; you
add files to `tooling/tools/`.

### `memory/` — five distinct memory subsystems

Files (top-level): `mod.rs`, `common.rs`, and one folder per subsystem:
`workspaces/`, `long_term/`, `vector/`, `memgraph/`, `rag/`.

Memory is the largest submodule and the one most likely to need
extending. It's covered in its own per-subsystem docs under
[extending-the-harness/memory/](./extending-the-harness/memory/index.md):

* `memory/index.md` — the overview, three-tier model (short-term +
  workspace + long-term plus graph + RAG layers).
* `memory/long-term.md` — global cross-workspace memory; importance
  scoring; recency decay.
* `memory/workspaces.md` — folder-anchored RAG workspaces; MRU; file
  indexing.
* `memory/vector-store.md` — the pure-Rust vector store underneath
  long-term and RAG.
* `memory/memgraph.md` — the Neo4j graph client (entities, chunks,
  relations) over direct Bolt.
* `memory/rag.md` — the four-tier RAG semantic store; hybrid retrieval;
  merge_and_rank.

The cross-cutting piece is `memory/common.rs::TEST_ENV_LOCK`, a single
process-wide mutex every memory submodule's `test_support.rs`
re-exports. Tests that override `WYLDE_DATA_DIR` must hold this lock
or two tests will trample each other's tmp dirs. This was a real
incident — see
`~/.claude/projects/.../memory/feedback_avoid_oncelock_for_test_env.md`.

### `model_registry/` — model identity and routing

Files: `mod.rs`, `types.rs`, `api.rs`, `heuristics.rs`, `hf_scanner.rs`,
`service_manifests.rs`, `routing/{mod,profiles,slots,churn,hf_search}.rs`,
`wakeword_scanner.rs`.

The model registry is the unified catalogue of every model the harness
might use: LLMs (Ollama), STT (whisper), TTS (kokoro), vision, embed,
wakeword. It merges three sources of truth: HuggingFace cache scan
results, live Ollama tags (via the `OllamaProbe` trait), and
service-manifest declarations.

Extension points:

* **`Kind` enum** (`types.rs:17–50`) — `Llm`, `Stt`, `Tts`, `Vision`,
  `Embed`, `Wakeword`. Adding a new kind is breaking — every consumer
  that filters on kind has to be updated. Update `KIND_VALUES` and
  `default_chat_visible` together.
* **Routing profiles** (`routing/profiles.rs`) — LLM-only; persisted at
  `data/model_registry/profiles.json`. Profile schema mirrors Python.
  Add a new profile field by extending the struct + writing migration.
* **`OllamaProbe` trait** (`api.rs:30–37`) — abstraction for "ask Ollama
  what models it has." Tests use `NullProbe`; production uses
  `LivePipeProbe`. Swap for a custom impl (e.g. HTTP-direct) by
  implementing the trait and passing it to `list_models_with_probe`.
* **HF discovery** (`routing/hf_search.rs`) — gated by
  `MODEL_DISCOVERY_ENABLED=true`. Off by default; enabling it adds
  HuggingFace API lookups to fill in missing model metadata.
* **Churn-prevention constants** (`routing/mod.rs:34–49`) —
  `MIN_DELTA_PCT`, `INCUMBENT_BONUS`, `MIN_BENCHMARK_RUNS`,
  `MAX_SWAP_PER_WEEK`, `FALLBACK_DAYS`. Tuning these changes how
  willingly the router swaps models. Coordinate with Python parity
  before changing.

Strangler-fig status: `WYLDE_HARNESS_MODEL_REGISTRY_IMPL` defaults to
`python`; flip to `rust` when Phase 8 parity tests pass.

### `pipe/` — the IPC dispatcher

Files: `mod.rs`, `chat.rs`, `tools.rs`, `memory_long_term.rs`,
`memory_workspaces.rs`.

The pipe submodule registers every `chat.*` / `tools.*` /
`memory.long_term.*` / `memory.workspaces.*` action on
`\\.\pipe\wylde-harness`. Handlers themselves live in the underlying
subsystems (e.g. `turn::actions`, `memory::workspaces::actions`); the
pipe modules are thin envelope wrappers.

This is mostly mechanical — adding a new verb means adding a handler in
the right subsystem and registering it in the matching pipe file. Note
the native gpui GUI no longer goes through this pipe for unary verbs:
since Phase 12.1 it calls the harness in-process via the
`wylde_harness::HarnessApi` short-circuit, with the GUI-side dispatch
files hoisted into the `wylde-gui-pipe` crate
(`Core/GUI/Frontend/Pipe/src/{chat,tools,memory_long_term,memory_workspaces}.rs`).
The harness binary still serves `\\.\pipe\wylde-harness` for MCP/CLI
clients. See [extending-the-gui.md](./extending-the-gui.md) for the full
GUI-facing story.

## How to extend

### Decide which submodule to touch

| You want to… | Touch… |
| --- | --- |
| Add a tool the model can call | `tooling/tools/` (see LLM tools doc) |
| Change how the model's output is parsed | `turn/salvage.rs` |
| Add a streaming event type | `events.rs` (and downstream consumers) |
| Add a new memory subsystem | `memory/` (and its own doc) |
| Add a new model kind (e.g. `Diffusion`) | `model_registry/types.rs` |
| Swap how the harness probes Ollama | implement `OllamaProbe` |
| Add a new GUI-facing pipe verb | `pipe/<file>.rs` + handler in subsystem |
| Change how tools are gated by tier | `turn/tool_round.rs::check_tier_gate` |

### Minimal example: adding a streaming event variant

Suppose you want to add a `ModelSwapped` event so the GUI can show
"switched from gpt-oss:20b to qwen:7b mid-turn." Two files change:

`events.rs` — add a new variant:

```rust
#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TurnEvent {
    TurnStarted { turn_id: String },
    Chunk { delta: String },
    ToolCall { name: String, args: Value },
    ToolResult { name: String, result: Value },
    ModelSwapped { from: String, to: String, reason: String }, // new
    TurnEnded { turn_id: String },
}
```

`turn/tool_round.rs` (or wherever the swap decision lives) — emit it:

```rust
sender.send(TurnEvent::ModelSwapped {
    from: old_model.clone(),
    to: new_model.clone(),
    reason: "vram_pressure".to_string(),
}).await?;
```

GUI consumer (`Core/GUI/Frontend/Panels/Chat/src/ipc.rs`) — handle the
new variant. The GUI mirrors `TurnEvent` as `TurnChunk` and decodes
incoming chunks in `TurnChunk::from_value`; unknown `type` tags fall
through to `TurnChunk::Unknown`, so a new variant is silently dropped by
the GUI until you add a matching arm there. The Rust compiler will tell
you which exhaustive `match` blocks on the harness side need a new arm,
but the GUI mirror is a separate enum and won't fail to compile — wire
it up by hand.

Test it: run the harness, drive a chat that forces a model swap, watch
the event stream. There's no isolated test harness for streaming events
yet — manual smoke is the convention until one lands.

## Gotchas

* **Don't add to `mod.rs` files without updating `lib.rs`.** Rust's
  module system is strict — a new file in `memory/something/` needs
  `pub mod something;` in `memory/mod.rs` and either a re-export or a
  consumer in `lib.rs`. The compiler catches missing `mod` declarations
  but not unused re-exports; clippy with `-D warnings` will.
* **`OnceLock` and `once_cell::Lazy` are traps for env-driven config.**
  Tests rebind `WYLDE_DATA_DIR` per test; cached values stick. Re-read
  env per call. See
  `~/.claude/projects/.../memory/feedback_avoid_oncelock_for_test_env.md`.
* **`TEST_ENV_LOCK` is one mutex shared by every memory submodule.**
  Holding it from `long_term/test_support.rs` blocks
  `workspaces/test_support.rs` from acquiring it. This is intentional;
  the alternative is racing tmp dirs. Don't try to per-submodule it.
* **The salvage parser's pattern priority matters.** Fenced JSON
  beats tag-wrapped beats bare-brace. A model that emits both a fenced
  block and a tag-wrapped one will have the fenced one win. If you add
  a new pattern, slot it in priority order and write a parity test —
  the Phase 5.D parity gate (25 cases in `rust/tests/parity/`) caught
  a real edge case here.
* **The harness is one crate by design.** Phase 5/6/7 went through a
  brief phase where per-area crates were considered (`wylde-harness-turn`,
  `wylde-harness-memory`) and abandoned. Submodules are right; new crates
  for harness concerns are wrong. The exception is when a concern
  genuinely belongs to a different process (the memgraph supervisor
  stays Python because it owns the JVM; the model-registry stays here
  because it has no separate process).

## Cross-links

* [extending-wylde.md](./extending-wylde.md) — system-level audience
  model and three-pillar overview.
* [extending-wylde-llm-tools.md](./extending-wylde-llm-tools.md) — the
  most common harness extension surface; touches `tooling/tools/`.
* [extending-the-gui.md](./extending-the-gui.md) — touches `pipe/` and
  the native gpui GUI (`wylde-gui` Shell).
* [extending-the-harness/memory/index.md](./extending-the-harness/memory/index.md)
  — the memory subsystem deep-dive.
* `docs/wylde-repo-organization.md` §3 — the harness deep dive in the
  canonical repo-organization reference.
* `docs/wylde-rust-migration-master-plan.md` — phase numbers (5, 6, 7,
  9, 11) trace back here.

---

*The harness is "one logical thing." When you're extending it, ask
first: am I changing how the chat brain itself works, or am I adding
a capability the chat brain can use? The latter is almost always a
tool — and almost always one file.*
