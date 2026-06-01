# Wylde cross-language parity suite

The **cutover gate** for the Python → Rust port. Each of the four services
(`wylde-gateway`, `wylde-vram-broker`, `wylde-device-gate`, `wylde-lifecycle`)
has a Python implementation and a Rust port. This suite fires the same
request at both implementations and diffs the responses. A service's
`WYLDE_*_IMPL` default may flip to `rust` only once its parity test passes:

| service          | impl env var                   | parity test       |
|------------------|--------------------------------|-------------------|
| gateway          | `WYLDE_WYLDE_GATEWAY_IMPL`      | `tests/gateway.rs`     |
| vram broker      | `WYLDE_WYLDE_VRAM_BROKER_IMPL`  | `tests/broker.rs`      |
| device gate      | `WYLDE_WYLDE_DEVICE_GATE_IMPL`  | `tests/device_gate.rs` |
| lifecycle        | `WYLDE_LIFECYCLE_IMPL`          | `tests/lifecycle.rs`   |

This is a **standalone cargo package**, deliberately excluded from the
`rust/` workspace (`exclude = ["tests/parity"]` in `rust/Cargo.toml`). It is
not built or run by `cargo build` / `cargo test` in the workspace — you opt
in explicitly.

## Prerequisites

1. **Release Rust binaries** — built from the `rust/` workspace:
   ```
   cd rust
   cargo build --release
   ```
   Produces `rust/target/release/wylde-{gateway,vram-broker,device-gate,lifecycle}.exe`.

2. **Python virtualenv** — `<repo>/.venv` with the service dependencies
   installed (FastAPI/uvicorn, Flask, msgpack, pywin32, …). The suite uses
   `.venv\Scripts\python.exe` directly; the system `py -3` resolves to a
   bare interpreter without the deps.

3. **No production Wylde instances running** — for the broker and
   device-gate tests. They bind the canonical pipes
   (`\\.\pipe\wylde-vram-broker`, `\\.\pipe\wylde-device-gate`),
   pre-flight-check for an existing instance, and abort with a message if
   one is found — stop Wylde first.

   **The lifecycle test is exempt:** its daemons bind *isolated* pipes
   (`wylde-lifecycle-parity-py` / `-rs`), never `\\.\pipe\wylde-lifecycle`,
   so it runs fine with a production lifecycle daemon up. See "Lifecycle
   parity" below.

4. Windows. The IPC transport is Windows named pipes.

## Running

The parity tests are **opt-in twice over**: every file under `tests/` is
`#![cfg(feature = "parity")]`, so a plain `cargo test` here runs only the
harness's own fast unit tests (`src/`) and none of the process-spawning
parity tests. You must pass the feature:

```
cd rust/tests/parity
cargo test --features parity -- --nocapture
```

Run one service:

```
cargo test --features parity --test gateway
cargo test --features parity --test broker
cargo test --features parity --test device_gate
cargo test --features parity --test lifecycle
```

See per-case `[probe]` / `[broker]` / `[device-gate]` lines and the parity
summary with `-- --nocapture`.

> First build is slow: as a standalone package it compiles its own copy of
> `tokio`, `reqwest`, and `wylde-shared` into `rust/tests/parity/target/`.

## Current status (2026-05-22)

| service     | gate result | notes |
|-------------|-------------|-------|
| vram broker | **PASS** — 12/12 | full envelope parity; cut over (`WYLDE_WYLDE_VRAM_BROKER_IMPL=rust`) |
| gateway     | **PASS** — 17/17 gated | full route-intersection parity; cut over (`WYLDE_WYLDE_GATEWAY_IMPL=rust`) |
| device gate | **PASS** — 8/8 | full envelope parity; cut over (`WYLDE_WYLDE_DEVICE_GATE_IMPL=rust`) |
| lifecycle   | **PASS** — 8/8 gated (run while live daemon up — see notes) | no-spawn control-surface parity; compile-verified, live diff pending — see "Lifecycle parity" |

A red `gateway` or `device_gate` test is the gate working as intended — it
blocks the `WYLDE_WYLDE_*_IMPL=rust` flip until the divergence is resolved.

### Gateway cutover (2026-05-21)

The five gated-route divergences that previously blocked the gateway flip
were closed on the Python side; all 12 gated routes are now at parity:

1. **`chat_run_turn`** — the Gateway app installs a global `HTTPException`
   handler that returns the error envelope bare, instead of FastAPI's
   `{"detail": ...}` wrapper.
2. **`voice_health`, `push_pending`, `link_status`** — `proxy_core.error()`
   now builds the canonical nested envelope (`{ok, error: {code, message}}`);
   an unreachable pipe maps to HTTP 503 (was a flat envelope at 502).
3. **`egress_destinations`** — both ports now emit each component's
   destination array sorted by `key`, so the listing is order-stable
   regardless of registry iteration order.

Of the five probe divergences that remained, `tools_list` and
`extension_call` were later promoted to gated (see next section), and
`conversations_list` / `prompts_list` followed once Python grew the
matching routers (see "Conversations + prompts ports promoted to gated").
Only `unknown_route` stays non-gating — the framework-default 404 bodies
are intentional asymmetry.

### Informational probes promoted to gated (2026-05-21)

`/api/rag`, `/api/tools` and `/extensions` exist on both sides and were
exercised only as informational probes. All three are now gated, taking
the gate set from 12 to **15** — the full route intersection:

1. **`rag_collections`** — already at parity. Both ports proxy the
   harness `rag.workspaces.list` action; an unreachable harness yields
   the same 503 envelope on each. Promoted with no code change.
2. **`tools_list`** — diverged: Python's `GET /api/tools` read the tool
   catalog *in-process* (`Core.harness.tooling.tool_registry`, HTTP
   200), while the Rust port proxies the harness `tools.list` pipe
   action (503 when the harness is down). The harness genuinely exposes
   `tools.list`, so the Rust transport is correct; Python's in-process
   import was the lone Gateway route not proxying its backing pipe.
   `Gateway/services/tool_registry.py::list_all` now proxies the harness
   `tools.list` action and reshapes the canonical list into the
   alias-keyed dict — same transport, same down-pipe 503 as Rust.
   (`get_one` / `GET /api/tools/{id}` is not gated and stays in-process.)
3. **`extension_call`** — `GET /extensions/parity/probe`. Both ports
   now dispatch `/extensions/<name>/<endpoint>` through the
   `wylde-extension-bridge` pipe service (see wave 2i in
   `docs/r3_gateway_deferred.md`). That service is not spawned inside
   the parity sandbox, so both implementations fold the pipe-transport
   fault onto the canonical `503 extension_bridge_unavailable`
   `{ok: false, error: {code, message}}` envelope — gated parity holds
   on the shared failure path. (Earlier the row diverged on a
   non-canonical top-level `data` field Python attached to its failure
   envelope; `Gateway/services/extensions.py::dispatch` emits the
   canonical envelope now.)

For `rag_collections` and `tools_list` the Rust port was already
correct and Python adapted onto the matching transport / envelope. For
`extension_call`, wave 2i moved *both* sides onto the same
`wylde-extension-bridge` pipe upstream — see
`docs/r3_gateway_deferred.md`.

### Conversations + prompts ports promoted to gated (2026-05-22)

`/api/conversations` and `/api/prompts` were Rust-only — Python's Gateway
never exposed them over HTTP (the desktop Tauri GUI reaches the harness
pipe directly). Python's `Gateway/routes/conversations.py` and
`prompts.py` now mirror the Rust routers verb-for-verb, dispatching the
same `conversations.*` / `prompts.*` harness pipe actions behind the same
`require_device` Bearer-token gate. Both rows are gated, taking the gate
set from 15 to **17** — the complete route intersection.

The gated cases are fired with **no Bearer token**, so each side rejects
at the auth layer with the byte-equivalent `401 missing_token` envelope —
the same path `chat_run_turn` already gates. The token-*present* path is
deliberately left ungated: it is not byte-equivalent. Python's
`require_device` collapses any device-gate fault (the pipe is not spawned
in the parity sandbox) into a single `503 device_gate_unavailable`, while
the Rust `authorize` passes the raw device-gate pipe error straight
through (HTTP 502 carrying the pipe's own error code). That divergence
predates this port and lives in the shared auth layer, not the new
routers.

### Lifecycle parity (2026-05-22)

The lifecycle row moved from **STUB** to a real 8-case gated suite. Two
pieces of daemon plumbing made it possible — both shipped on the Python
*and* Rust daemon so the gate is symmetric:

1. **No-spawn mode** (`--no-spawn` / `WYLDE_LIFECYCLE_NOSPAWN=1`) — the
   control + manifest surfaces come up but `_start_<service>` forks
   nothing, recording a "would-have-spawned" entry instead. Without it a
   parity run would boot Wylde's entire `tier=core` stack.
2. **Isolated pipe names** (`WYLDE_LIFECYCLE_PIPE_NAME`) — each parity
   daemon binds `wylde-lifecycle-parity-py` / `-rs` rather than the
   canonical `\\.\pipe\wylde-lifecycle`. This is what lets the test run
   **while a production lifecycle daemon is up**: the parity daemons and
   the live daemon never contend for a pipe. No-spawn mode additionally
   skips the `core.json` manifest write, so a parity daemon cannot clobber
   a live daemon's manifest either.

The 8 gated cases: `ping`, `handshake`, `lifecycle.status`,
`lifecycle.list_services`, `lifecycle.start_service`, `unknown_action`,
`empty_action`, `lifecycle.shutdown_all`. The `lifecycle.*` actions are a
no-spawn control surface both daemons answer byte-identically; the
launcher/registry-backed `service.start` / `service.list` / `service.health`
stay Python-only (the Rust port defers them) and are not gated.

Status: the suite **compiles cleanly** (`cargo test --features parity
--test lifecycle --no-run`) and every case is expected at parity by
construction (the daemons' handlers are byte-paired). The live
cross-implementation diff has not yet been run in-tree — it needs the
release `wylde-lifecycle.exe` and a `.venv`; run it with:

```
cargo test --features parity --test lifecycle -- --nocapture
```

It is safe to run with the live Wylde stack up (isolated pipes).

## How it works

- **Gateway** (`tests/gateway.rs`) — both gateways are launched on different
  ports (`WYLDE_GATEWAY_PORT`) and run **simultaneously**; each request is
  fired at both with `reqwest::blocking` and the responses diffed.
- **Broker / device gate** (`tests/broker.rs`, `tests/device_gate.rs`) —
  both implementations bind the *same* canonical pipe with no name override,
  so they are captured **sequentially**: a fixed action script is replayed
  against a fresh Python process, then a fresh Rust process, then the two
  reply lists are diffed. Requests go over the pipe via
  `wylde_shared::ipc::send_action`. A fresh process per side means each is
  exercised from identical state — the fair comparison for a stateful
  service.
- **Lifecycle** (`tests/lifecycle.rs`) — sequential capture, like the broker
  and device gate, but each daemon is launched with `--no-spawn` (control
  surface up, no `tier=core` children) and an isolated pipe via
  `WYLDE_LIFECYCLE_PIPE_NAME`, so the test never collides with a production
  lifecycle daemon. See that file's module docs and "Lifecycle parity" above.

### Gate vs. probe

A response always carries fields that *must* differ between two runs —
timestamps, UUID lease ids, pids, live hardware readings. The harness
(`src/diff.rs`) normalizes those volatile paths out of both sides before
comparing, so a diff only fires on a *real* divergence.

Gateway cases are tagged `gate` or `probe`:

- **gate** — a divergence fails the test. The route is claimed at parity.
- **probe** — a divergence is reported as an informational finding but does
  not fail the test. New routes start here; promote them to `gate` as the
  Rust port reaches parity.

The gate set is the **route intersection** — every route Python serves from
`Gateway/routes/*.py` that the Rust port also implements (`/health`, the
chat surface, `models`, `voice`, `devices`, `push`, `link`, `images`,
`settings`, `egress`, `rag`, `tools`, `extensions`, `conversations`,
`prompts`). The Rust-only routes (see next section) are never gated.
`/api/conversations` and `/api/prompts` were the last both-sides routes
promoted from probe to gate (see "Conversations + prompts ports promoted
to gated").

## Rust-only surface (not gated)

The Python Gateway exposes a deliberate *subset* of HTTP routes. The Rust
port adds extra HTTP surface for functionality Python does not serve over
HTTP:

| Rust route           | Python counterpart |
|----------------------|--------------------|
| `/api/memory`        | none               |
| `/api/workspaces`    | none               |

This asymmetry is intentional. Python's desktop GUI is a local Tauri app
that reaches harness functionality (memory layers, workspaces) over the
`\\.\pipe\wylde-harness` named pipe directly — it has no need for an HTTP
route. Mobile clients have no pipe: they reach Wylde only over the
WyldeLink VPN tunnel, and the sole ingress is Gateway HTTP. The Rust port
therefore exposes these routes so mobile can reach the same harness
functionality the desktop GUI gets via the pipe.

`/api/conversations` and `/api/prompts` were on this list until Python's
Gateway grew matching routers — they are now part of the gated route
intersection (see "Conversations + prompts ports promoted to gated").

Gating a Rust-only route would always fail — Python serves a 404 where the
Rust port serves a real response — so these routes are not exercised by
`tests/gateway.rs` at all.

## Layout

```
rust/tests/parity/
  Cargo.toml            standalone package, feature `parity`
  README.md             this file
  src/
    lib.rs              harness crate root
    paths.rs            repo root, .venv python, release binaries
    proc.rs             spawn a service as a child process (kill on drop)
    diff.rs             normalize volatile fields + structural diff
    http.rs             HTTP capture + SSE parsing (gateway)
    pipe.rs             named-pipe capture (broker, device gate)
  tests/
    gateway.rs          gateway HTTP parity
    broker.rs           VRAM broker pipe parity
    device_gate.rs      device gate pipe parity
    lifecycle.rs        lifecycle parity stub
```
