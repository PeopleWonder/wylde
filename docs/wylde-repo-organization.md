---
title: Wylde Repository Organization Reference
audience: the Wylde user + future Claude sessions
authored: 2026-05-25
status: living reference — update on phase ships
---

# Wylde Repository Organization

A point-in-time reference for "where does X live, why, what are the conventions, what's migrating to where." Written after the 2026-05-25 ship cluster (Phase 5.D cutover, Phase 6 tooling, Phase 7.A workspaces, Phase 7.B long-term + memgraph + RAG, Phase 11.B TTS, shutdown-reaper + lifecycle-sandbox fixes). Trust the filesystem first; the memory files this borrows from drift faster than the repo does.

## TL;DR

**Wylde is a personal, local-first AI assistant platform.** It runs as a constellation of services on a single Windows machine, talking to each other through Windows named pipes (`\\.\pipe\wylde-<service>`) using a msgpack-over-pipe envelope. Each service owns one concern — Gateway brokers external HTTP, Voice does STT/TTS, Memgraph wraps a bundled Neo4j JVM, the Harness owns the chat brain (turn driver + tooling + memory), Lifecycle is the daemon that spawns and supervises everyone, Trainer does fine-tuning and captioning, VPN provides the WyldeLink mesh, the resource_monitor / vram-broker arbitrates GPU memory, and a native gpui (Rust) desktop GUI sits on top. Browser extensions and remote peers are first-class but always traverse the Gateway (web) or the VPN (remote).

**The org is in the middle of a Python → Rust migration.** Every service was born in Python under `<Service>/` and `Core/<Service>/`. The Rust ports live in `rust/crates/wylde-<service>/` and are wired in as strangler-fig replacements gated on a `WYLDE_<SERVICE>_IMPL=python|rust` env var. The default for most services has flipped to `rust` (lifecycle, gateway, voice — and Phase 5.D just flipped the chat-turn driver); the memory layer is the most active lane and still defaults to `python`. Python remains the rollback path on a 14-day soak before deletion. The Harness has consolidated to **one logical thing** — one crate (`wylde-harness`), one binary, one pipe, with submodules for turn/tooling/memory rather than per-phase crates.

**Two rules that shape every decision.** First: **Principle #16, single auth boundary at the VPN tunnel.** Gateway has exactly two tiers (`public` health endpoints, `local` everything else); once a peer is tunneled into the WyldeLink mesh they appear as a local caller, no per-route API keys. Second: **the harness is one crate, one binary, one pipe.** Past plans envisioned per-phase Rust crates (`wylde-harness-turn`, `wylde-memgraph`); both have been retired in favour of submodules under `wylde-harness/src/<area>/`. When a memory file or doc references a separate crate, suspect it is stale.

---

## Table of contents

1. [Top-level repo layout](#1-top-level-repo-layout)
2. [The `rust/` Cargo workspace](#2-the-rust-cargo-workspace)
3. [`wylde-harness` deep dive](#3-wylde-harness-deep-dive)
4. [`Core/` Python layout](#4-core-python-layout)
5. [Top-level service folders](#5-top-level-service-folders)
6. [External runtimes](#6-external-runtimes)
7. [Service architecture conventions](#7-service-architecture-conventions)
8. [Strangler-fig migration conventions](#8-strangler-fig-migration-conventions)
9. [`data/` directory layout](#9-data-directory-layout)
10. [`docs/` directory](#10-docs-directory)
11. [Auto-memory system](#11-auto-memory-system)
12. [The `wylde_check` linter](#12-the-wylde_check-linter)
13. [The gpui GUI (`Core/GUI/`)](#13-the-gpui-gui-coregui)
14. [Test layout conventions](#14-test-layout-conventions)
15. [Build + distribution](#15-build--distribution)
16. ["Where do I look?" service map](#16-where-do-i-look-service-map)
17. [Glossary](#17-glossary)
18. [Things to read next](#18-things-to-read-next)

---

## 1. Top-level repo layout

The repo root contains everything, and it **is** a git repository: trunk is `develop` (the default branch), `main` is stable-only, and both are protected by rulesets (PR-only, required checks). Version history lives in git — use `git log`, not the `docs/` lineage, as the record of what changed when.

> **Stale-path note (scrubbed for #31).** This section used to state that the root was
> `%USERPROFILE%\Documents\Obsidian Vault\Wylde\`, that there was no `.git/`, and that
> "`git status` will refuse" — so version history was implicit in progress-memory files. All of
> that is now false: the tree moved out of the Obsidian vault and is under git. Paths in this doc
> are deliberately **repo-relative** rather than absolute, so they don't rot the same way again.

| Folder | Purpose |
| --- | --- |
| `rust/` | The backend Cargo workspace. All Rust service ports, the workspace `Cargo.toml`, build artifacts, parity-test suite, spikes. (The GUI is a *separate* Cargo workspace under `Core/GUI/` — see §13.) |
| `Core/` | The Python "core" — anything cross-service, supervisor, harness brain, Memgraph supervisor, lifecycle daemon, shared IPC. Also hosts `Core/GUI/`, which is *not* Python — it is the standalone gpui (Rust) GUI workspace (§13). |
| `Voice/` | Voice service (Python): wake-word, STT, TTS orchestration. Rust port lives in `rust/crates/wylde-voice/`. |
| `VPN/` | VPN service (Python): WyldeLink mesh, NAT discovery, peer pairing, monitoring. Rust port at `rust/crates/wylde-vpn/`. |
| `Gateway/` | Legacy Python Gateway (Flask). The active Gateway is the Rust crate; Python kept as rollback. |
| `N8N/` | N8N integration: workflow templates + Python tool wrappers. |
| `Extensions/` | Browser-extension hosts (Webcrawler, Wylde_Study) + the `extension_bridge` service. |
| `device_gate/` | Per-device authentication service (Python). Rust port at `rust/crates/wylde-device-gate/`. |
| `data/` | Runtime state owned by services: manifests, contracts, model registry, persisted memory, settings. |
| `docs/` | Plans, handoffs, design docs, archive. |
| `logs/` | Per-service rotating log dirs. |
| `verification/` | Diagnostic scripts (`check_venv.py` etc.) — sanity checks for the dev environment. |
| `.venv/`, `.wylde/`, `.mypy_cache/`, `.pytest_cache/`, `.ruff_cache/` | Tooling caches. `.venv/` is uv-managed Python 3.11. |
| `launch_wylde.ps1` | Boot script — activates venv, launches the lifecycle daemon. |
| `pyproject.toml`, `uv.lock` | Python deps via uv with per-service extras (voice, trainer, harness, memgraph, vpn, …). |
| `phase7_smoke.bat`, `WYLDE_ENDPOINTS.md`, `Nextcloud.url` | Misc top-level helpers. |
| `README.md` | Setup + uv extras table + boot recipe. |

There is no separate `Wylde/` directory at the top — what the migration plan once called "Wylde/Core/" is just `Core/` now. Likewise the GUI is **inside** Core (`Core/GUI/`), not a top-level peer. Note `Core/GUI/` is the one exception to "all Rust lives in `rust/`": it is its **own** Cargo workspace (gpui-native desktop app), deliberately not nested in the `rust/` workspace — see §13.

---

## 2. The `rust/` Cargo workspace

`rust/Cargo.toml` defines the workspace with resolver 2. **Current workspace members (post-2026-05-25 trim):**

```
crates/wylde-shared          — IPC, manifest, logging, secure-file primitives
crates/wylde-vram-broker     — GPU-memory leasing arbitrator
crates/wylde-device-gate     — per-device auth
crates/wylde-gateway         — Axum-on-127.0.0.1:8005, the unified HTTP boundary
crates/wylde-lifecycle       — daemon supervisor (spawns + heartbeats peers)
crates/wylde-ollama          — Ollama proxy + lease integration
crates/wylde-vpn             — WyldeLink mesh + NAT + pairing + tunnel
crates/wylde-extension-bridge — browser-extension MCP host
crates/wylde-harness         — chat brain (turn + tooling + memory)
crates/wylde-voice           — STT + TTS pipe service
build-support/wylde-prebuild-guard — opt-in build-script helper, see §15
```

**`wylde-memgraph` was removed from the workspace on 2026-05-25.** The memgraph client is now an in-process submodule of the harness (`wylde-harness/src/memory/memgraph/`), so the standalone service crate is no longer needed. The bundled Neo4j JVM stays supervised by Python (`Core/Memgraph/`) until a follow-up slice migrates that lifecycle ownership. A directory at `rust/crates/wylde-memgraph/` may still exist on disk as leftover scaffolding but it is no longer compiled.

**`tests/parity/`** is an excluded workspace sibling, not a member — `cargo test` does not run it. The `wylde-parity` package gates cross-language byte-shape parity tests behind a `parity` cargo feature, pulls in `pretty_assertions`, and depends on the *built* service binaries. Run with `cargo test --features parity` after building both sides. The live suites are `lifecycle` (Python ↔ Rust no-spawn surface) and `wylde-ollama` (live-Ollama smoke); the gateway/broker/device-gate/vpn and Phase 5.D salvage-parser suites were retired as each Python half was deleted (see the crate README's "Retired suites").

### Per-crate quick reference

| Crate | Pipe | Default impl | Strangler env | What it owns |
| --- | --- | --- | --- | --- |
| `wylde-shared` | (library) | n/a | n/a | `ipc::{client,server,wire,actions,observability}`, `manifest`, `manifest_status`, `secure_file`, `logging`. The msgpack envelope, the named-pipe server/client, and the action registry every other crate plugs into. |
| `wylde-vram-broker` | `\\.\pipe\vram-broker` | rust | (Python deleted) | `service.rs` / `workers.rs` / `policy.rs` — lease-based VRAM arbitration. Tracks per-worker leases, evicts on policy, exposes `lease.acquire/release`. |
| `wylde-device-gate` | `\\.\pipe\wylde-device-gate` | rust | n/a | Per-device bearer tokens + auth store; gates inbound device pairing. |
| `wylde-gateway` | `\\.\pipe\wylde-gateway` + HTTP `127.0.0.1:8005` | rust | n/a | Axum app, route modules per area (`chat`, `voice`, `rag`, `memory`, `models`, `images`, `tool_registry`, `extensions`, …), egress allowlist + kill switch, mcp surface, auth tiers (public/local — see Principle #16). |
| `wylde-lifecycle` | `\\.\pipe\wylde-lifecycle` | rust | `WYLDE_WYLDE_LIFECYCLE_IMPL` | Daemon supervisor; `control.rs` for start/stop, `daemon.rs` for spawn-and-heartbeat, `registry.rs` for service slots, `state/services.rs` per-service slots. The Rust impl is canonical; the Python `Core/Lifecycle/daemon.py` is kept as the rollback path. |
| `wylde-ollama` | `\\.\pipe\wylde-ollama` | rust | n/a | Proxies the local Ollama HTTP API behind the lease primitive; `actions/` hosts the chat / generate / embed surface. |
| `wylde-vpn` | `\\.\pipe\wylde-vpn` | python | `WYLDE_WYLDE_VPN_IMPL` | `tunnel/`, `nat/`, `pairing.rs`, `peers/`, `discovery/`, `monitoring/`. WyldeLink mesh + mDNS + WireGuard wrapper. |
| `wylde-extension-bridge` | `\\.\pipe\wylde-extension-bridge` | rust | n/a | Browser-extension host + MCP surface (`mcp/`) — discovers per-extension manifests, routes requests. |
| `wylde-harness` | `\\.\pipe\wylde-harness` | rust (5.D flip) | `WYLDE_WYLDE_HARNESS_IMPL` and `WYLDE_HARNESS_IMPL` | The chat brain. See §3. |
| `wylde-voice` | `\\.\pipe\wylde-voice` | rust | n/a | Whisper STT (`transcribe/`) + Kokoro TTS (`synth/`) + cpal mic + openWakeWord + lease integration. 8 GUI-facing voice actions ported in Phase 11.E. **Rust-only since the Phase 11.E cutover** — the Python `Voice/` tree was deleted, the `WYLDE_WYLDE_VOICE_IMPL` / `WYLDE_VOICE_IMPL` rollback knob dropped, and the live session STT/TTS paths moved in-process (the orchestrator calls `voice.transcribe` / `voice.synthesize` directly instead of round-tripping the retired harness `models.transcribe` / `models.synthesize`). |

`wylde-shared` is the only library-only crate; every other crate has both `lib.rs` and `main.rs` and ships as a service binary. The `actions/` (or single `actions.rs`) module per crate is where pipe verbs are registered onto the shared `ipc::server` registry.

---

## 3. `wylde-harness` deep dive

The harness is the Wylde user's "one logical thing" — one crate, one binary, one pipe, submodules for distinct concerns. Layout of `rust/crates/wylde-harness/src/`:

| Path | Owns |
| --- | --- |
| `config.rs` | Process-wide config (model registry path, data dir resolution, feature flags). Reads env per-call, not via `OnceLock` — see [feedback-avoid-oncelock-for-test-env](../../.claude/projects/.../memory/feedback_avoid_oncelock_for_test_env.md). |
| `dispatch.rs` | The tool-call routing layer. `call_internal` routes to `tooling::runner::dispatch_tool`; `call_mcp_extension` routes to the extension bridge. The Phase 6 fix replaced `call_internal_stub` (which returned `not_implemented` for every tool) with a real registry-backed dispatcher. |
| `events.rs` | Wire types for streaming events — `TurnStarted`, `Chunk`, `ToolCall`, `ToolResult`, `TurnEnded`. The streaming-action protocol on top of shared IPC. |
| `state.rs` | Per-turn `TurnState` + `new_turn_id()` (32-char hex, Rust shape — Python's was uuid4 with hyphens). The streaming-side turn registry lives here. |
| `service.rs` | `install()` / `stop()` / `reset_for_tests()`. Registers every `chat.*`, `memory.*`, `meta.*`, `tool.*` action on the shared IPC registry. `ALL_ACTIONS` array enumerates everything that gets registered. |
| `main.rs` | Binary entry — slice tag (currently `7.B`), manifest write, heartbeat loop, serve loop. The serve-loop attestation (`mark_serve_loop_entered`) lives here. |
| `lib.rs` | `pub mod` declarations + re-exports of `install` / `stop` / `reset_for_tests`. |
| `turn/` | Chat-turn driver (Phase 5). `mod.rs` for `chat.run_turn`, `actions.rs` for the action handlers + `build_alias_map()`, `salvage.rs` for the tool-call salvage parser (fenced/tagged/bare JSON), `tool_round.rs` for the tool-call loop + tier gate. |
| `tooling/` | In-process tool registry (Phase 6). `registry.rs` is the catalog (`global()` accessor); `runner.rs` is `dispatch_tool()`; `tools/` has one file per Python tool group. |
| `tooling/tools/` | `fs.rs`, `diff.rs`, `search.rs`, `meta.rs`, `time_tools.rs`, `memory.rs`, `rag.rs`, `deferred.rs`. Each file registers its tools with `destructive: bool` flags + `HandlerKind::{Active, Deferred{phase,reason}}`. |
| `memory/` | The memory layer (Phase 7). See below. |
| `memory/common.rs` | Shared helpers: `data_dir()`, `registry_path()`, `settings_path()`, **`TEST_ENV_LOCK`** — a single mutex re-imported by every memory sub-module's `test_support.rs` so cross-module tests don't race on `WYLDE_DATA_DIR`. |
| `memory/workspaces/` | Phase 7.A — registry-only workspace store (`store.rs`, `mru.rs`, `slug.rs`, `actions.rs`). 8 pipe actions (`memory.workspaces.{list,recent,get,…}`). No LanceDB dependency. |
| `memory/long_term/` | Phase 7.B subtask 1 — JSON-authoritative long-term memory + bincode vector mirror. `records.rs`, `scoring.rs`, `entries.rs`. Strangler default still `python`; Rust handlers registered. |
| `memory/vector/` | Phase 7.B subtask 1 — pure-Rust vector store. Single-file bincode envelope (`StoreOnDisk { version, dim, records: Vec<Record> }`), atomic `.tmp+rename` persist, linear cosine scan. Chosen over HNSW crates because long-term memory is curated (low thousands of records max). |
| `memory/memgraph/` | Phase 7.B subtask 2 + direct-Bolt cutover (2026-05-26) — graph client. `bolt.rs` (neo4rs Bolt connection pool), `cypher.rs` (typed query helpers), `client.rs` (typed wrappers for health/ensure_schema/upsert/delete/traverse/relate/multihop/upsert_edge/stats), `schema.rs` (label + relation constants), `graph_retrieval.rs` (`expand_by_graph()` hybrid expander), `actions.rs` (`meta.graph_query`). **Current transport: direct Bolt** via neo4rs — `WYLDE_HARNESS_MEMORY_IMPL` default flipped python→rust on 2026-05-26. The Python `Core/Memgraph/` service stays only to supervise the bundled Neo4j JVM. See memory file `wylde_memgraph_direct_bolt.md`. |
| `memory/rag/` | Phase 7.B subtask 3 (shipped 2026-05-25) — tiered RAG. `store.rs` (JSON-authoritative tiered records + bincode vector mirror sibling of long_term), `tiers.rs` (4 tiers: core/episodic/semantic/procedural), `search.rs` (`search`, `search_logged`, `search_with_graph` hybrid composer), `merge.rs` (`_merge_and_rank` vector+graph fusion), `miss_log.rs` (telemetry), `prune.rs` (filtered destructive cleanup), `feedback.rs` (CITED_IN + RETRIEVAL_MISS edge writeback), `ingest.rs` (N8N webhook trigger — transport-deferred), `actions.rs` (eight `rag.*` model-callable tools). The hybrid graph+vector path is wired into `meta.graph_query` so the entity-only path is now hybrid by default. |

After the 2026-05-25 ship cluster the harness sits at **372 tests** (was 188 → 292 with long_term → 301 with memgraph → 372 with RAG). The strangler-fig is still `WYLDE_HARNESS_MEMORY_IMPL=python` for the whole `memory/` subtree — the Rust handlers are reachable through the in-process tool catalog, but the canonical pipe traffic still goes through `Core/harness/memory/*.py` until parity-test gates land per submodule.

### `wylde-harness` integration tests

Top-level `tests/` files under the crate:

* `run_turn_loop_e2e.rs` — multi-iteration tool loop against a stub Ollama-like.
* `tool_dispatch_e2e.rs` — registry dispatch end-to-end.
* `memgraph_integration.rs` — one `#[ignore]` smoke test that needs a live `\\.\pipe\wylde-memgraph` + a `pipe_connect` always-on error-envelope test.

---

## 4. `Core/` Python layout

`Core/` is mostly the Python side. Some pieces are canonical and load-bearing (lifecycle, memgraph supervisor), some are strangler-fig fallbacks (harness turn driver, harness memory), some are shared libraries the Rust services do not yet have equivalents for (`Core/shared/`). The one non-Python resident is `Core/GUI/` — a self-contained gpui (Rust) workspace that happens to live under `Core/` for historical reasons (see §13).

| Path | Status | Notes |
| --- | --- | --- |
| `Core/Lifecycle/` | **canonical** | The daemon supervisor. `daemon.py` is the entry; `daemon_state/` has `_services.py` (slot definitions), `_services_basic.py` / `_services_harness.py` / `_services_trainer.py` (per-group start/stop), `_orphan_sweep.py` (the manifest-orphan reaper — see [wylde-shutdown-orphan-reaper](../../.claude/projects/.../memory/wylde_shutdown_orphan_reaper.md) and rule 31), `_strangler.py` (impl-flag resolution), `_manifest.py`, `__init__.py` (the `stop_all_daemon_managed` shutdown path). `Core/Lifecycle/tests/` has the autouse sandbox fixture (rule 32). The Rust port at `rust/crates/wylde-lifecycle/` is the default; the Python daemon is kept as the rollback path. |
| `Core/Memgraph/` | **canonical (supervision only)** | Supervises the bundled Neo4j JVM (`vendor/`). The harness talks to Neo4j over **direct Bolt** (`bolt://127.0.0.1:7687`) — the former Python pipe/IPC clone (`run.py`, `ipc/`) and the `graph_service/` Cypher routes were removed when the direct-Bolt path became canonical; only the JVM supervision remains here. |
| `Core/GUI/` | **canonical (not Python)** | The native gpui (Rust) desktop app — its own Cargo workspace, not part of the `rust/` workspace and not Python at all. Lives under `Core/` for historical reasons only. See §13. |
| `Core/shared/` | **canonical (shared lib)** | `consul_client.py`, `discovery.py`, `errors.py`, `ipc/`, `logging_setup.py`, `manifest.py`, `secure_file.py`, `system_prompts*.py`, `tool_interface.py`, `vram_broker.py`. The Python-side mirror of `wylde-shared`. |
| `Core/harness/` | **strangler-fig** | Python harness. `turn/` (driver — replaced by `wylde-harness/src/turn/`), `tooling/` (registry + tools — replaced by `wylde-harness/src/tooling/`), `memory/` (workspaces, long_term, memgraph, RAG, embeddings, reflection — being replaced module-by-module under `wylde-harness/src/memory/`), `pipe/` (action handlers — fewer of these as Rust takes over), `backend/` (Ollama client + request building + streaming + response normalization), `model_registry/`, `prompts/`, `server.py`, `dev/wylde_check/`. Phase 5.D scheduled deletion is 2026-06-08 (14-day soak from cutover). |
| `Core/Config/` | **canonical** | YAML config: `auto_mode.yaml`, `embeddings.yaml`. |
| `Core/Network/` | **canonical** | `services.yaml` — service identity registry. |
| `Core/resource_monitor/` | **deleted (Rust-only)** | The Python broker + Flask probe (`run.py`, `broker/`, `vram_broker_service.py`) were deleted in `7072947` once the Rust `wylde-vram-broker` passed a live function test; it is now the sole broker with no Python fallback. The directory holds only untracked `__pycache__/` cruft + `data/hardware.json` locally (nothing git-tracked). |

The big rule for Core/: **anything still serving live pipe traffic is canonical, anything strangler-figged is on a soak clock.** Check the manifest for `entry_point` — `python:Core.X.run` means Python is still canonical; `rust:wylde-X` means the Rust binary is canonical and Python is the rollback path.

### `Core/harness/` — Python module ↔ Rust submodule map

| Python module | Rust replacement | Status |
| --- | --- | --- |
| `Core/harness/turn/_driver.py` + `_streaming.py` + `_tool_round.py` + `_state.py` + `_request_build.py` + `_end_of_turn.py` | `wylde-harness/src/turn/` (`mod.rs`, `tool_round.rs`, `salvage.rs`, `actions.rs`) | **flipped 5.D 2026-05-25**, Python deletion 2026-06-08 |
| `Core/harness/tooling/tool_registry/` + `tool_runner/` + `tools/` | `wylde-harness/src/tooling/` (`registry.rs`, `runner.rs`, `tools/`) | **flipped Phase 6** — 10 active tool ids, ~50 deferred stubs, alias map populated from registry |
| `Core/harness/memory/workspaces/` | `wylde-harness/src/memory/workspaces/` | Phase 7.A shipped, Rust handlers registered, strangler default `python` |
| `Core/harness/memory/long_term.py` | `wylde-harness/src/memory/long_term/` + `vector/` | Phase 7.B-1 shipped, default `python`, reindex required on cutover |
| `Core/harness/memory/memgraph.py` | `wylde-harness/src/memory/memgraph/` (bolt.rs + cypher.rs + client.rs) | **flipped python→rust 2026-05-26** via direct-Bolt cutover; Python pipe rollback only |
| `Core/harness/memory/rag.py` + `vector_store.py` + `miss_log.py` + `rag_feedback.py` + `ingest.py` + `rag_*.py` | `wylde-harness/src/memory/rag/` | Phase 7.B-3 shipped 2026-05-25, default `python`, 8 RAG tools active in registry |
| `Core/harness/memory/reflection.py` (~619 LOC) | (not yet ported) | future 7.B+ slice — importance-promotion + chain-pruning passes |
| `Core/harness/memory/workspace_memory/` | (not yet ported) | future 7.B+ slice — `memory_workspace_save` tier still deferred |
| `Core/harness/memory/scheduler.py` | (not yet ported) | Phase 7.F |
| `Core/harness/backend/*` (Ollama client, request building, streaming, response normalization) | `wylde-ollama` plus harness-internal `turn::tool_round` | covered piecewise; Ollama proxy in Rust; the backend's "request building" is harness-side |
| `Core/harness/model_registry/` | (not yet ported) | future slice |
| `Core/harness/server.py` (the run-loop) | `wylde-harness/src/main.rs` | Rust binary is the runner when `WYLDE_WYLDE_HARNESS_IMPL=rust` (default since 5.D) |
| `Core/harness/dev/wylde_check/` | (no Rust port planned) | Pure-Python dev tool, runs off the filesystem — intentional Python-permanent. |

---

## 5. Top-level service folders

These directories pre-date the migration. Each contains a `run.py`, a `manifest.json`, and tests. Most are now booted via the Rust binary when `WYLDE_<SERVICE>_IMPL=rust`; the Python tree stays as rollback.

* **`Voice/`** — Python orchestrator (now **rollback only** post-Phase-11.E cutover, 2026-05-27). `wake_word.py` (Porcupine-style detector), `transcribe.py` (faster-whisper STT wrapper), `synthesize.py` (kokoro-onnx TTS wrapper), `orchestrator.py` (text→phoneme + STT/TTS pipeline glue), `audio_io.py`, `device_manager.py`, `record.py`, `state.py`, `pipe.py`. Rust port (`wylde-voice`) now owns STT + Kokoro TTS + cpal mic + openWakeWord; the 8 GUI-facing voice actions ported in Phase 11.E. Python deletion scheduled 2026-06-10 to 2026-06-24.
* **`VPN/`** — WyldeLink mesh. `api.py`, `tunnel/`, `nat/`, `peers/`, `pairing/`, `discovery/`, `monitoring/`, `entrypoint.sh`, `start_wylde_vpn.bat`. Implements Principle #16's auth boundary. Rust port at `wylde-vpn` exists.
* **`Trainer/`** — *Extracted from the alpha 2026-06-04 — see `docs/retired-trainer-scope.md`.* Held the `Caption/` captioning sub-tool plus the top-level LLaMA-Factory wrapper and the `wylde-trainer` Rust pipe; deferred as a separate project, restorable from git `68ef1d1`.
* **`Gateway/`** — Legacy Python Flask gateway. The active gateway is the Rust crate (`wylde-gateway`); this tree is the rollback. `app.py`, `routes/`, `middleware/`, `egress/`, `services/`, `streaming.py`, `proxy_core.py`, `_audit/`, `secrets/`, `auth/`. Browser-extension routing started here (`extension_routes.py`) but has moved into the Rust crate's `routes/extensions.rs`.
* **`N8N/`** — Workflow engine integration. `client.py` (HTTP client to the bundled N8N), `tools/` (Python tool wrappers), `workflow_templates/`. Note from the mypy-strict memory ([feedback_strict_mypy_catches_latent](../../.claude/projects/.../memory/feedback_strict_mypy_catches_latent.md)): the Python `N8N/tools/*` imports `from Wylde.N8N.client import …` which doesn't actually resolve at runtime; every call has been silently falling through to its ImportError envelope. Real fix waits on either the import-path correction or a Rust port.
* **`Extensions/`** — Browser-extension hosts and the bridge. `extension_bridge/` is the service (`run.py`, `dispatcher.py`, `loader.py`, `registry.py`, `pipe.py`, `contract.py`) backed in Rust by `wylde-extension-bridge`. `Webcrawler/` and `Wylde_Study/` are per-extension MCP servers + tool registries (`mcp-server.json`, `handler.py`, `tools/`, `tests/`). `Wylde_Study/browser_extension/` is the actual Chrome/Brave extension source.
* **`device_gate/`** — Per-device auth service. `auth.py`, `core.py`, `pipe.py`, `run.py`, `store.py`, `data/`. Rust port at `wylde-device-gate`.

There is no separate `Caption/` directory at the top level — it lives at `Trainer/Caption/`.

---

## 6. External runtimes

These are the **only** non-Rust runtime dependencies Wylde owns at the platform level. Each is wrapped by a Rust or Python supervisor. (The GUI used to belong here as a "Svelte + Tauri WebView" runtime; post-cutover it is a plain gpui Rust binary with no webview runtime of its own, so it dropped off this list — see §13.)

| Runtime | Role | Wrapped by | Pipe / port |
| --- | --- | --- | --- |
| **Memgraph (Neo4j JVM bundle)** | Graph database — entities, chunks, relations (CALLS / IMPORTS / INHERITS / CONFIGURES / EXPOSES / MENTIONED_IN). | `Core/Memgraph/` (Python supervisor for the JVM). Harness clients talk to Neo4j directly via Bolt — the legacy `\\.\pipe\wylde-memgraph` msgpack pipe is rollback-only as of the 2026-05-26 cutover. | Bolt 7687 (canonical); `\\.\pipe\wylde-memgraph` (rollback) |
| **Ollama** | Local LLM inference runtime. | `wylde-ollama` Rust crate (proxy + lease integration) | `\\.\pipe\wylde-ollama`, upstream Ollama HTTP 11434 |
| **N8N** | Workflow / automation engine. | `N8N/` Python (HTTP client) | HTTP 5678 (bundled) |

When the GUI needs the LLM it goes gpui panel → `wylde-gui-pipe` (in-process `wylde_harness::HarnessApi` short-circuit for unary verbs, or the named pipe otherwise) → harness → ollama pipe — no webview hop. When tooling needs the graph it goes harness → memgraph pipe → Bolt → Neo4j. When training fires off captioning it goes Trainer pipe → caption worker → torch.

---

## 7. Service architecture conventions

Every Wylde service has the same shape; deviations are bugs the `wylde_check` rules catch. The shape is:

**Manifest** at `data/manifests/<service>.json` — `service`, `version`, `pipe`, `port`, `category`, `description`, `entry_point` (`python:Module.run` or `rust:crate-name`), `contributes` (dashboard color/icon/label), `startup_sequence` (the four phases below), `shutdown_attested` flag, and a live `status` block (`pid`, `started_at`, `heartbeat`, `state`, `last_seen`).

**Contract** at `data/contracts/actions/<service>.json` — declares the action verbs the service registers on its pipe, with per-action docstring + handler-module pointer. The GUI's contract-checker (`wylde_check` rule 9: `gui_action_contract`) reads these to validate that every `pipeAction(SVC_X, "x.y", …)` call in the GUI hits a real handler.

**Pipe** at `\\.\pipe\wylde-<service>` — msgpack-framed envelopes via `wylde_shared::ipc` (Rust) or `Core/shared/ipc/` (Python). The envelope shape is v1 (Rust) for the harness; some legacy Python pipes still emit a hand-rolled v0 shape. Cross-language IPC works because the shared crate accepts both.

**Lifecycle spawn** — the daemon (`Core/Lifecycle/daemon_state/_services_*.py`) picks Python vs Rust off `WYLDE_<SERVICE>_IMPL` (single underscore form) or `WYLDE_WYLDE_<SERVICE>_IMPL` (double underscore = daemon-spawn decision). The double-underscore form decides which binary the daemon spawns; the single-underscore form decides whether a Python forward shim hands traffic to the Rust pipe. Both must agree on the cutover or you get the Python service running but Python pipe traffic forwarding to a Rust pipe that isn't there.

**Startup sequence** (the four phases every `run.py`/`main.rs` must execute, enforced by rule 18: `run_py_startup_sequence`):

1. `configure_logging()` — only via `Core/shared/logging_setup` (rule 13).
2. `write_manifest()` — record pid + start time. Single write per service (rule 2 catches double-writes).
3. `start_heartbeat()` — background tick that bumps `manifest.status.heartbeat`.
4. `serve_loop` — the pipe acceptor. **Must call `mark_serve_loop_entered()`** which sets `shutdown_attested: false` and signals "I made it past startup." Phase 5.D fixed a bug where the attestation wasn't fired; rule check for `serve_loop` attestation is still spotty per the `wylde_phase7b_memgraph_shipped` writeup (two known-deferred warnings on memgraph + voice manifests).

**Shutdown handler** (rule 19) — `run.py` must register SIGTERM/SIGINT (Python) or graceful-shutdown (Rust) whose body updates the manifest with a stopped state.

**Manifest-orphan reaper at shutdown** — `Core/Lifecycle/daemon_state/__init__.py::stop_all_daemon_managed` MUST call `reap_manifest_orphans()` from `_orphan_sweep.py`. Without it, services from prior crashed daemon sessions stay alive (a `wylde-gateway.exe` survived 14 days of restarts before the 2026-05-25 fix). Rule 31 (`shutdown_reaps_manifest_orphans`) enforces. The reaper is the safety net, not the primary path — tracked Popen handles still drain first with a graceful CTRL_BREAK_EVENT.

---

## 8. Strangler-fig migration conventions

**Env-var pattern.** Each migration target gets an env var pair. `WYLDE_<SERVICE>_IMPL` (Python forward shim — does the Python action handler hand traffic to the Rust pipe?). `WYLDE_WYLDE_<SERVICE>_IMPL` (daemon spawn — does the lifecycle daemon spawn the Python module or the Rust binary?). Defaults are conservative; values clamp to `python` on unknown input. Some legacy single-var forms exist (`WYLDE_HARNESS_TURN_IMPL` → `WYLDE_HARNESS_IMPL` 2026-05-24 consolidation rename).

**Default stays python until parity proven.** A slice ships when:
1. Rust impl + tests are green.
2. `cargo clippy --all-targets -- -D warnings` clean.
3. `mypy` + `ruff check` clean on the parallel Python.
4. `wylde_check` clean for the slice's files.
5. The Rust handlers are registered (reachable through the tool catalog) but the env-var default is `python`.

**Parity gate before flip.** A separate slice writes byte-shape parity tests under `rust/tests/parity/tests/<area>.rs`. The live example is `tests/lifecycle.rs` — the Python lifecycle daemon's no-spawn control surface diffed against the Rust port. Only after the parity gate is green does the env-var default flip to `rust`. When a service's Python half is later deleted its parity suite is retired with it (no second implementation left to diff) — see the crate README's "Retired suites".

**14-day soak before Python deletion.** Once the default flips, the Python implementation stays on disk as the rollback path. the Wylde user and the daemon logs are checked for "python-fallback" path firing during the soak. Phase 5.D flipped 2026-05-25 → Python `Core/harness/turn/` scheduled deletion 2026-06-08 earliest. The pre-deletion verification recipe lives in `docs/wylde-phase5-cutover.md`.

**Release builds blocked when fresh manifests are live.** The prebuild guard (`build-support/wylde-prebuild-guard/`) refuses to start a Rust build while a Python service holds a fresh manifest — locked `.exe` files in `rust/target/` would error. Stop the stack from the tray before `cargo build --release`.

---

## 9. `data/` directory layout

```
data/
├── .initialized                — boot sentinel
├── contracts/
│   └── actions/<service>.json  — action surface (verb → handler module + doc)
├── manifests/<service>.json    — live status, pid, heartbeat
├── model_registry/             — model identity / routing (currently empty on disk)
├── settings.json               — top-level settings
└── state/
    ├── stopped                 — sentinel: services were intentionally stopped
    └── vram-broker.json        — broker lease state
```

`data/manifests/` currently has entries for `vram-broker`, `wylde-device-gate`, `wylde-gateway`, `wylde-memgraph`, `wylde-voice`. Live services as of writing: voice (`alive`), memgraph (`alive`), broker (`alive`); gateway shows `dead-orphan` (the 14-day-survivor zombie that motivated the reaper fix). `data/contracts/actions/` has entries for vram-broker, device-gate, extension-bridge, gateway, lifecycle, trainer, voice, vpn — the harness and memgraph contracts are written elsewhere (the harness's `ALL_ACTIONS` array is the source of truth for its surface).

The harness memory layer also reads/writes `data/memory/` (workspaces dir, `long_term.json`, `long_term.vec.bin`, RAG tiered store + `.vec.bin` mirror, `miss_log.jsonl`). These paths are not constants — they are derived from `data_dir()` per call and so are sandboxable in tests (see §14).

---

## 10. `docs/` directory

The planning + handoff dir. Important entries:

* `wylde-rust-migration-master-plan.md` — the multi-phase master plan from earlier 2026. Phase numbers (5, 6, 7, 11, 12) trace back to this. The shipped slices' memory files reference it for spec sheets.
* `wylde-rust-phase7-handoff.md` — the Phase 7 handoff written 2026-05-25 after 7.A shipped. Documents the 7.B/7.C/7.D/7.E/7.F slice breakdown.
* `wylde-phase5-cutover.md` — the full writeup of the 5.D chat-turn flag flip + pre-deletion verification recipe.
* `wylde-passwords-self-healing-extension.md` — ~7,300-word design doc for the planned NC Passwords fork (click-to-inject + AI self-healing rule loop). B0–B7 roadmap, ~7–10 weeks solo. Unblocked post-Phase-6.
* `wylde-android-app-plan.md` — mobile autofill / companion app plan.
* `wylde-ollama-design.md` — design doc for the Rust ollama proxy.
* `wylde-voice-npu-spike-findings.md` — findings from the NPU spike (ORT load-dynamic gotchas, Kokoro's CPU EP requirement).
* `privacy-plan.md` — privacy roadmap (the passwords extension is line item §3.3).
* `manifest_ownership.md`, `mcp_surface.md`, `dev_setup.md`, `MIGRATING_EXTENSIONS.md`, `r3_gateway_deferred.md`, `mypy_baseline.md` + `.txt`, `mypy_strict_mode_completion.md` — operations / cross-cutting docs.
* `extending-wylde.md` (+ `-llm-tools`, `-services`, `-extensions`, `extending-the-gui.md`) — the five-doc "how to extend Wylde" set, authored 2026-05-27. Codifies the actions-registry / three-dispatchers model and the services-vs-extensions distinction. Read `extending-wylde.md` first.
* `extending-the-harness.md` + `extending-the-harness/memory/` (`index.md`, `long-term.md`, `workspaces.md`, `vector-store.md`, `memgraph.md`, `rag.md`) — the six harness-deep-dive docs, authored 2026-05-27. Cover the five harness submodules (turn / tooling / memory / model_registry / pipe) and the five memory subsystems in depth. All written exec-summary-then-analysis style.
* `wylde_check_rules.md`, `wylde_check_batch_bc_findings.md` — linter docs.
* `wylde-open-questions-research.md`, `wylde-pairing-future-cd.md` — open questions / future direction.
* `refactor-archive/`, `diagnostic-archive/` — historical artifacts; do not edit, use for context only.

---

## 11. Auto-memory system

the Wylde user's Claude sessions persist memory between conversations at
`%USERPROFILE%\.claude\projects\<repo-path-slug>\memory\`, where `<repo-path-slug>` is the repo's
absolute path with drive/separator characters replaced by `-` (for a checkout at
`C:\Users\<you>\Wylde\Core`, that is `C--Users-<you>-Wylde-Core`). The slug is **derived from wherever
the repo lives**, so it changes if the tree moves — it previously read
`C--Users-<user>-Documents-Obsidian-Vault-Wylde`, from the retired Obsidian-vault location (#31). The
directory is **outside the repo** — it's part of the Claude config, not version-controlled with Wylde.
Convention:

* Each memory is its own MD file with frontmatter (`name`, `description`, `metadata.type`).
* Types: `user` (who the Wylde user is + preferences), `feedback` (corrections + validated approaches), `project` (in-progress work + decisions), `reference` (pointers to external systems).
* `MEMORY.md` is the index — one line per memory: `- [Title](file.md) — one-line hook`. Loaded into every conversation context up to ~200 lines.
* `[[name]]` links connect related memories; a `[[name]]` to a non-existent memory marks something worth writing.
* Project / feedback memories use a Why: / How to apply: structure so judgement calls survive context shifts.

Stale-memory hygiene: when a memory is contradicted by the filesystem, the filesystem wins. Update the memory or delete it; don't fight the code.

---

## 12. The `wylde_check` linter

Lives at `Core/harness/dev/wylde_check/` — pure Python, no subprocesses, no network, walks the filesystem skipping `_legacy/`, `__pycache__/`, build output. **43 active rules** as of writing (rules 7/9/11/30 were retired at the slice-11 GUI cutover when the Svelte/Tauri trees were deleted; rules 44-47 were added in the same slice; original numbers are kept for surviving rules so cross-references stay stable). Registered in `rules/__init__.py` and split across:

* `rules/_runtime.py` — runtime / lifecycle invariants (manifest writes, spawn paths, pipe-name convention, startup-sequence, shutdown handler, the reaper rule, the sandbox rule).
* `rules/_quality.py` — code-quality invariants (file size limits, test init present, docstring required, etc.).
* `rules/_arch.py` — architectural invariants (import paths, dead service refs, gateway scope, memory layer boundaries).
* `rules/_gui.py` — surviving GUI rule (rule 10, `gui_no_backend_bypass`, repointed at the cutover from the deleted Svelte/Tauri trees to the gpui `Core/GUI/Frontend` + `Core/GUI/Shell` source). The Svelte-era rules it used to hold (inference-bar purity, action contract, pipe constants) were retired.
* `rules/_gpui.py`, `rules/_gpui_contract.py`, `rules/_gpui_nav.py`, `rules/_gpui_workspace.py`, `rules/_gpui_polish.py` — the gpui GUI contract rules (panel verbs exist in the harness registry, first-party manifest must be a gpui View, no cross-panel imports, no legacy GUI imports in panels, WebView only in extension handlers, panel crate must be a workspace member, `stream_call` must handle cancel, …).
* `rules/_rust.py` — Rust-side invariants (no external process spawn from inside crate src/, import path conventions).
* `rules/_actions.py` — action-registry invariants.
* `rules/_tools.py` — tool-id regex + tool docstring required.
* `_config.py` — allowlists + exemptions.
* `_walkers.py` — the tree walker.
* `_single_file.py` — `check_one_file()` for pre-write hooks.

Add a new rule by: (1) writing the check function in the appropriate `_<area>.py`, (2) registering it in `rules/__init__.py`, (3) updating the numbered docstring at the top of the package `__init__.py`. Rule findings carry a stable id + severity (`error` / `warning` / `info`) and surface in the `wylde_check` envelope as `{ok, data: {findings, summary}, error?}`.

The most-loaded rules right now: **rule 31** (`shutdown_reaps_manifest_orphans`) and **rule 32** (`manifest_sandbox_required`) — both added 2026-05-25 in response to live incidents. Rule 14 (`no_external_subprocess`) is the spawn-restriction. Rule 16 (`run_py_entry_point`) enforces that every service folder uses `run.py` as its entry. Rule 17 (`pipe_name_convention`) — `^wylde-[a-z][a-z0-9-]*$`.

---

## 13. The gpui GUI (`Core/GUI/`)

`Core/GUI/` is a **native gpui (Rust) desktop application** as of the slice-11 cutover (2026-05-29). The previous Tauri 2 + Svelte 5 alpha was deleted in that cutover — there is no more `src/` (Svelte SPA), `src-tauri/` (Tauri shell), `package.json`, `tauri.conf.json`, Vite, npm, `node_modules/`, or WebView2 runtime. The GUI talks to Wylde pipes **directly from Rust** via `wylde-gui-pipe`; for unary verbs it uses an in-process `wylde_harness::HarnessApi` short-circuit (Phase 12.1) that bypasses the IPC hop entirely, while the harness binary still serves its named pipe for MCP / CLI clients. No HTTP for local traffic, no dev server. The full migration rationale lives in `docs/wylde-gpui-rewrite-plan.md` (a *migration* doc — its own Tauri/Svelte references describe what was replaced).

**Its own Cargo workspace.** `Core/GUI/Cargo.toml` is a standalone workspace (`resolver = "2"`), deliberately **not** nested in the `rust/` workspace. The reason is blast-radius containment: gpui pulls in heavy graphics deps whose version-unification could ripple into the backend lockfile and trigger unrelated version bumps. Keeping it separate keeps `rust/Cargo.lock` untouched. gpui is not on crates.io — it is pinned to git rev `b3d93d44` of `github.com/zed-industries/zed`.

**Top-level contents of `Core/GUI/`:**

* `Cargo.toml` / `Cargo.lock` — the standalone GUI workspace + its own lockfile.
* `manifest.json` — service manifest; `entry_point` is `Core/GUI/target/release/wylde-gui.exe`, `tier: core`, `shutdown_order: 10`.
* `Shell/` — the only binary crate (package `wylde-gui`, `bin` at `src/main.rs`); this is the shipped GUI binary.
* `Frontend/` — the library crates the Shell links (theme, pipe, input widget, WebView host, and the panels).
* `Manifest/Extension_handlers/` — `wylde-panel-registry` (panel manifest schema v2 aggregator + runtime overlay + `gui.list_tabs`; ships a `wylde-panel-aggregator` bin).
* `installer/` — WiX/NSIS installer scaffolding (currently just `README.md`).
* `assets/` — bundled assets (e.g. `icons/`).
* `docs/` — GUI-local docs (the historical inference-bar audit + migration plan; see below).
* `target/` — gpui build output (`.gitignore`'d).

**The `Shell/` crate** (`Shell/src/`, package `wylde-gui`) is the gpui app entry + top-level chrome:

* `main.rs` + `lib.rs` — gpui `Application` boot.
* `window.rs` — window + chrome.
* `shell_root.rs` — the top-level layout root.
* `sidebar.rs` + `nav.rs` + `slot.rs` — the left nav, navigation, and the panel slot the active panel renders into.
* `tray.rs` — system tray via the `tray-icon` crate (replaces `tauri::tray`); the tray "Quit" and window-close both trigger manifest-driven shutdown.
* `shutdown.rs` — the clean-shutdown path (drives each service's `shutdown_order` from its `manifest.json`).
* `assets.rs` + `pack.rs` — bundled fonts / brand assets / icon set.

**`Frontend/` crates:**

| Path | Crate | Owns |
| --- | --- | --- |
| `Frontend/Theme/` | `wylde-theme` | Color tokens + Inter typography. The one "shared" crate every panel imports for visual cohesion. |
| `Frontend/Pipe/` | `wylde-gui-pipe` | IPC primitives. `chat.rs` (streaming chat), `memory_long_term.rs`, `memory_workspaces.rs`, `tools.rs` (per-area pipe-call helpers), `nav_bus.rs` (cross-panel navigation bus), plus `stream_call` (ChunkFrame streaming with abort-on-drop, enforced by wylde_check `stream_call_must_handle_cancel`). |
| `Frontend/Input/` | `wylde-gpui-input` | A `TextInput` widget — gpui at rev `b3d93d44` ships no built-in text input. |
| `Frontend/Extension_handlers/WebView/` | `wylde-webview` | `wry`-based WebView host for extension iframe panels (the slice 12.7 `ui_panels` field). Loopback-validated; the *only* place a WebView is allowed (wylde_check `webview_only_in_extension_handlers`). |
| `Frontend/Panels/<Name>/` | `wylde-panel-<name>` | The 11 first-party panels, each a gpui `View` crate (a workspace member, enforced by `panel_crate_must_be_workspace_member`). |

The 10 first-party panel crates: `Settings` (`wylde-panel-settings`), `Workspaces` (`wylde-panel-workspaces`), `Tools` (`wylde-panel-tools`), `Memory` (`wylde-panel-memory`), `Chat` (`wylde-panel-chat`), `Models` (`wylde-panel-models`), `Dashboard` (`wylde-panel-dashboard`), `Devices` (`wylde-panel-devices`), `RemoteAccess` (`wylde-panel-remote-access`), `Images` (`wylde-panel-images`). Each panel sub-crate is typically `lib.rs` + a `<name>_panel.rs` (the `impl Render` gpui View) + `ipc.rs` (its pipe-call helpers) + a `manifest.json` registry entry. (The `Training` panel was extracted 2026-06-04 — see `docs/retired-trainer-scope.md`.)

**Panels are gpui Views, not Svelte components/pages.** A first-party panel's manifest `factory` resolves a gpui `View` (enforced by wylde_check `first_party_manifest_must_be_gpui_view`). Extensions contribute panels via the `ui_panels` manifest field (slice 12.7); those are iframe panels hosted in `wylde-webview`, loopback-only. The aggregator (`wylde-panel-aggregator` in `Manifest/Extension_handlers/`) globs every panel manifest, the runtime registry overlays extension panels via `extensions.list_panels`, and `gui.list_tabs` exposes the unified set to the nav.

**Architecture rules** (wylde_check, see §12): `no_cross_panel_imports` (a panel can't import a sibling panel crate), `no_legacy_gui_imports_in_panels` (no references to the deleted Tauri/Svelte tree), `webview_only_in_extension_handlers`, `first_party_manifest_must_be_gpui_view`, `panel_crate_must_be_workspace_member`, `stream_call_must_handle_cancel`.

**Historical inference-bar docs.** `Core/GUI/docs/inference-bar-migration-plan.md` + `inference-bar-audit.md` describe the old Svelte `InferenceBar.svelte` tool-call loop and the plan to evacuate its backend mechanics into the harness. They are **historical**: the Svelte file they describe no longer exists (the agent loop now lives in `wylde-harness/src/turn/`), but the docs remain on disk as lineage. Treat them as background, not as current GUI structure.

---

## 14. Test layout conventions

**Rust unit tests** — `#[cfg(test)] mod tests` inside each `.rs` file. The harness test count (~372) is dominated by these. Test helpers that mutate `WYLDE_DATA_DIR` use a per-test guard plus the shared `TEST_ENV_LOCK` mutex in `memory/common.rs` so cross-module tests don't race.

**Rust integration tests** — `tests/` under each crate. The harness has `run_turn_loop_e2e.rs`, `tool_dispatch_e2e.rs`, `memgraph_integration.rs`. Tests that need a live service pipe are gated `#[ignore]` so plain `cargo test` doesn't hit them. Voice's `jfk_end_to_end.rs` needs `ORT_DYLIB_PATH`, `WYLDE_IPC_DISABLE=1`, and `WYLDE_VOICE_STT_ENCODER_PATH` (the env-var block is in the test itself; copy it for new live-model tests in that crate).

**Cross-language parity tests** — `rust/tests/parity/` is excluded from the default workspace, gated on `--features parity`, and depends on built service binaries plus a working `.venv`. The live suites are `lifecycle` and `wylde-ollama`; the gateway/broker/device-gate/vpn and Phase 5.D harness/turn suites were retired with their deleted Python halves (see the crate README).

**Python pytest** — under `Core/Lifecycle/tests/`, `Core/harness/tests/`, `Core/shared/tests/`, per-service `<Service>/tests/`. Run with `uv run pytest Core Gateway "device_gate" Voice VPN Trainer N8N`.

**The autouse sandbox.** `Core/Lifecycle/tests/conftest.py` has two autouse fixtures every test under that tree inherits: `sandboxed_manifest_dir` (rebinds both `daemon_state._MANIFEST_DIR` and `Core.shared.manifest._MANIFEST_DIR` to a tmp dir per test) and `_force_kill_pid_watchdog` (wraps the kill helper to refuse any pid not registered as test-owned). This was added 2026-05-25 after a synthetic test pid coincided with a live `wylde-gateway.exe` pid and force-killed the running gateway. Rule 32 (`manifest_sandbox_required`) catches new tests that read `_MANIFEST_DIR` or `data/manifests/` without patching. Don't suppress the rule.

**Multi-root pytest pitfall.** `Core/` is a namespace package; both `Core/Lifecycle/tests/conftest.py` and `Core/shared/tests/conftest.py` resolve to `tests.conftest`, so `pytest Core/Lifecycle/ Core/shared/tests/` collides. Gate runs invoke each tree separately; the collision is latent.

**`OnceLock` warning.** Don't cache env-var-driven paths with `OnceLock` / `once_cell::Lazy` in Rust code under test. The first test caches a path, the rest see stale values. Re-read env per call; the cost is trivial compared to the disk IO that follows. See `feedback_avoid_oncelock_for_test_env.md`.

---

## 15. Build + distribution

**Rust** — `cargo build --release` from `rust/`. Outputs to `rust/target/release/wylde-<service>.exe`. The pre-build guard (`build-support/wylde-prebuild-guard/`) lives outside `crates/` because the spawn-restriction linter (`no_external_process_spawn_rust`) only walks `rust/crates/<crate>/src/`. The guard refuses to start if a wylde-* service holds a fresh manifest — locked .exe files would otherwise fail the build. Stop the stack from the tray before building.

**GUI (gpui)** — `cargo run -p wylde-gui` (run from `Core/GUI/`) for the dev cycle; `cargo build --release` from `Core/GUI/` produces the shipped binary at `Core/GUI/target/release/wylde-gui.exe` (the `entry_point` in `Core/GUI/manifest.json`). This is a **separate** workspace from `rust/` — building the backend does not build the GUI and vice-versa. No npm/Vite/Tauri CLI; there is no dev server. Installer scaffolding (WiX/NSIS) lives under `Core/GUI/installer/` (currently a stub).

**Python** — uv-managed. `.venv\Scripts\python.exe` is the canonical interpreter; `py -3` resolves to the system Python 3.14 and breaks on missing extras (the `passlib missing` symptom is almost always wrong interpreter, not torn venv). Run `verification/check_venv.py` to diagnose. Use `uv run` or `.venv\Scripts\python.exe` for Wylde commands.

**Distribution (Phase 12)** — planned but not in flight. The strangler-fig deletions reduce Python tree size first; Phase 12 will produce a packaged installer (WiX/NSIS, scaffolded at `Core/GUI/installer/`) that ships the gpui `wylde-gui.exe` + the Rust services + bundled Memgraph + Ollama dependency. The gpui binary carries no webview runtime, so the old "missing WebView2" first-run failure mode is gone.

---

## 16. "Where do I look?" service map

| I want to find… | Look at |
| --- | --- |
| The LLM inference pipe | `rust/crates/wylde-ollama/` → `\\.\pipe\wylde-ollama` |
| The chat-turn driver | `rust/crates/wylde-harness/src/turn/` (Rust, default since 5.D); Python fallback at `Core/harness/turn/` |
| The salvage parser (tool-call decode) | `rust/crates/wylde-harness/src/turn/salvage.rs` (Rust); `Core/harness/turn/_streaming.py` (Python rollback) |
| The tool registry | `rust/crates/wylde-harness/src/tooling/registry.rs` (`global()` accessor) |
| The active built-in tools | `rust/crates/wylde-harness/src/tooling/tools/{fs,diff,search,meta,time_tools,memory,rag}.rs` |
| Deferred tool stubs (with phase + reason) | `rust/crates/wylde-harness/src/tooling/tools/deferred.rs` |
| Long-term memory store | `rust/crates/wylde-harness/src/memory/long_term/` — JSON authoritative at `data/memory/long_term.json` + bincode vector mirror at `data/memory/long_term.vec.bin` |
| RAG tiered store | `rust/crates/wylde-harness/src/memory/rag/store.rs` — JSON + bincode mirror, four tiers (core / episodic / semantic / procedural) |
| RAG hybrid search (vector + graph) | `rust/crates/wylde-harness/src/memory/rag/search.rs::search_with_graph` + `merge.rs::_merge_and_rank` |
| Memgraph client | `rust/crates/wylde-harness/src/memory/memgraph/` — direct Bolt via neo4rs (`bolt.rs`, `cypher.rs`, `client.rs`). Flipped python→rust 2026-05-26. |
| Workspace registry | `rust/crates/wylde-harness/src/memory/workspaces/` — 8 `memory.workspaces.*` pipe actions |
| The vector store primitive | `rust/crates/wylde-harness/src/memory/vector/mod.rs` — pure-Rust, bincode + linear cosine |
| The Ollama upstream HTTP wrapper | `rust/crates/wylde-ollama/src/upstream.rs` |
| The VRAM lease primitive | `rust/crates/wylde-vram-broker/` + `wylde-ollama::lease`, `wylde-voice::lease` |
| HTTP routes (Gateway) | `rust/crates/wylde-gateway/src/routes/{chat,voice,rag,memory,images,models,extensions,…}.rs` |
| Egress allowlist + kill switch | `rust/crates/wylde-gateway/src/egress/` + `routes/egress.rs` |
| The shared IPC envelope | `rust/crates/wylde-shared/src/ipc/{wire,client,server,actions,observability}.rs` |
| The supervisor (daemon) | `Core/Lifecycle/daemon.py` + `Core/Lifecycle/daemon_state/_services_*.py` (Python canonical); rollback at `rust/crates/wylde-lifecycle/` |
| The manifest-orphan reaper | `Core/Lifecycle/daemon_state/_orphan_sweep.py` — called from `__init__.py::stop_all_daemon_managed` |
| The autouse test sandbox | `Core/Lifecycle/tests/conftest.py` |
| The wylde_check linter | `Core/harness/dev/wylde_check/` (32 rules in `rules/__init__.py`) |
| Service manifests (live status) | `data/manifests/<service>.json` |
| Service action contracts | `data/contracts/actions/<service>.json` |
| The GUI app entry (gpui Shell) | `Core/GUI/Shell/src/main.rs` + `lib.rs` (package `wylde-gui`) |
| The GUI tray + clean-shutdown trigger | `Core/GUI/Shell/src/tray.rs` + `shutdown.rs` |
| The GUI → pipe IPC layer | `Core/GUI/Frontend/Pipe/` (`wylde-gui-pipe`; `HarnessApi` short-circuit, `stream_call`, `nav_bus`) |
| A first-party GUI panel | `Core/GUI/Frontend/Panels/<Name>/` (gpui View, package `wylde-panel-<name>`) |
| The GUI panel registry / aggregator | `Core/GUI/Manifest/Extension_handlers/` (`wylde-panel-registry` + `wylde-panel-aggregator` bin) |
| The chat agent loop (was the GUI's InferenceBar) | `rust/crates/wylde-harness/src/turn/` — the Chat panel (`Core/GUI/Frontend/Panels/Chat/`) is now just a streaming-aware renderer |
| The Voice STT pipeline | `rust/crates/wylde-voice/src/transcribe/` (Rust, Whisper); `Voice/transcribe.py` (Python rollback) |
| The Voice TTS pipeline | `rust/crates/wylde-voice/src/synth/` (Rust, Kokoro phoneme path — 11.B); `Voice/synthesize.py` (Python rollback + text-path) |
| WyldeLink VPN | `rust/crates/wylde-vpn/` + `VPN/` (Python rollback) |
| Browser-extension bridge | `rust/crates/wylde-extension-bridge/` + `Extensions/extension_bridge/` |
| Webcrawler extension | `Extensions/Webcrawler/` |
| Wylde_Study extension | `Extensions/Wylde_Study/` |
| Device pairing / auth | `rust/crates/wylde-device-gate/` + `device_gate/` |
| Parity tests | `rust/tests/parity/` (gated `--features parity`) |
| Memgraph Cypher routes | `Core/Memgraph/graph_service/_routes_*.py` (server) — these are the canonical field names; the Rust client aligns to these, not to the Python client's payloads |

---

## 17. Glossary

* **Harness** — the chat brain. One crate (`wylde-harness`), one binary, one pipe. Hosts the turn driver, tool registry, memory layer, end-of-turn sweep.
* **Strangler-fig** — Martin Fowler's pattern: build the replacement alongside the original, gate traffic on a flag, flip the flag once parity is proven, delete the original after soak.
* **Soak** — the period (currently 14 days for chat-turn) between flipping the impl-flag default and deleting the old implementation. Watching for python-fallback log lines + rollback opportunities.
* **`serve_loop` attestation** — the `mark_serve_loop_entered()` call that flips `shutdown_attested: false` and signals "startup succeeded." Required by rule 18.
* **Parity test** — a byte-shape test that pins identical envelopes from Python and Rust impls for the same input. Gate before flipping the env-var default to `rust`.
* **MRU** — Most-Recently-Used. Workspace MRU at `data/memory/workspaces/settings.json` drives "recent" lists in the GUI.
* **Tier gate** — the harness check that a tool's `destructive: bool` flag is respected by the active permission tier (`tool_use` tier blocks destructive tools with `ToolErrorReason::TierReadOnly`).
* **Salvage parser** — the harness module that extracts tool calls from model outputs — fenced JSON, `<tool_call>…</tool_call>` tags, bare JSON, alias resolution, OpenAI-nested `function` field, Llama `parameters` field. Originally per-pattern Python regex in `_streaming.py`; ported to Rust in Phase 5.C (`turn/salvage.rs`).
* **MCP-style extension** — extensions register a `mcp-server.json` next to their `manifest.json` and surface tool ids through the extension-bridge. The harness routes them via `dispatch::call_mcp_extension`.
* **WyldeLink** — the VPN mesh that gives remote peers a CGNAT-style address (100.64.0.0/10) so they appear as local callers to the Gateway. Implements Principle #16.
* **Principle #16** — the design rule: one auth boundary, at the VPN tunnel. Gateway has two tiers, `public` (health only) and `local` (everything else). No per-route API keys.
* **Prebuild guard** — the build-script helper at `rust/build-support/wylde-prebuild-guard/` that blocks `cargo build --release` while wylde-* services hold fresh manifests (would otherwise lock the .exe files).
* **Manifest orphan** — a service from a prior crashed daemon session that is still alive but whose `_<service>_proc` slot in the new daemon is `None`. Reaped at shutdown by `_orphan_sweep.py`.
* **Action contract** — `data/contracts/actions/<service>.json`. Declares the verb surface a pipe exposes; the GUI's `pipeAction` calls are linted against it (rule 9).
* **Tier** (RAG) — one of `core`, `episodic`, `semantic`, `procedural`. Each tier has its own bucket in the tiered store.
* **`ToolErrorReason::TierReadOnly` / `phase_<n>_deferred`** — error reasons surfaced to the model when a tool is gated or not-yet-ported. The model sees clean "not yet" rather than `unknown_tool`.

---

## 18. Things to read next

For the migration trajectory and recent ship reports (in approximate read order):

1. `docs/wylde-rust-migration-master-plan.md` — the multi-phase plan.
2. `docs/wylde-rust-phase7-handoff.md` — the active phase, memory-layer detail.
3. `docs/wylde-phase5-cutover.md` — how a strangler flip actually looks in practice.
4. `~/.claude/projects/.../memory/wylde_phase5_5d_cutover.md` — 5.D cutover memo (env vars, parity gate, deletion clock).
5. `~/.claude/projects/.../memory/wylde_phase6_shipped.md` — Phase 6 tooling shape (registry, runner, tier gate).
6. `~/.claude/projects/.../memory/wylde_phase7b_long_term_shipped.md` — long-term + vector store rationale (why bincode, why linear scan).
7. `~/.claude/projects/.../memory/wylde_phase7b_memgraph_shipped.md` — memgraph harness client shape (read alongside the direct-Bolt refactor note).
8. `~/.claude/projects/.../memory/wylde_phase11_slice_11b_shipped.md` — TTS module pattern (canonical for extending voice).
9. `~/.claude/projects/.../memory/wylde_shutdown_orphan_reaper.md` + `wylde_lifecycle_test_manifest_sandbox.md` — the two 2026-05-25 incident fixes.
10. `docs/wylde-passwords-self-healing-extension.md` — the largest near-term net-new feature plan.
11. `docs/wylde-gpui-rewrite-plan.md` — the gpui GUI rewrite plan (source of truth for the post-cutover `Core/GUI/` layout; a *migration* doc, so its Tauri/Svelte references describe what was replaced). `Core/GUI/docs/inference-bar-migration-plan.md` survives as historical lineage of the now-deleted Svelte InferenceBar.

For the test + tooling discipline:

* `~/.claude/projects/.../memory/feedback_avoid_oncelock_for_test_env.md` — Rust env-var caching pitfall.
* `~/.claude/projects/.../memory/feedback_strict_mypy_catches_latent.md` — strict-mode mypy as a real-bug finder.
* `~/.claude/projects/.../memory/wylde_py3_resolves_to_python_314.md` — Python interpreter gotcha.
* `~/.claude/projects/.../memory/wylde_voice_test_env_vars.md` — live-model voice-test env block.

For the design principles:

* `~/.claude/projects/.../memory/wylde_principle_16_single_auth_boundary.md` — Principle #16.
* `docs/privacy-plan.md` — privacy roadmap.
* `docs/manifest_ownership.md`, `docs/mcp_surface.md` — cross-cutting surfaces.

---

*Last updated 2026-05-30: §13 rewritten for the slice-11 GUI cutover (2026-05-29) — `Core/GUI/` is now a native gpui (Rust) desktop app in its own Cargo workspace; the Tauri 2 + Svelte 5 alpha (`src/`, `src-tauri/`, npm/Vite) was deleted. The §6 external-runtimes list dropped the webview row, the §16 service map and §15 build instructions were repointed at the gpui workspace, and the §12 linter count was corrected to 43 active rules (GUI rules retired/repointed at the cutover). Prior revision (2026-05-27): Phase 11.E voice cutover, 2026-05-26 memgraph direct-Bolt cutover, the Phase 9 pipe central dispatcher, and the 11-doc extending-* family. Update on phase ships and major refactors; trust the filesystem when a memory or doc diverges from current state. The gpui layout's source of truth is `docs/wylde-gpui-rewrite-plan.md`.*
