# Wylde cross-language parity suite

The **cutover gate** for the Python → Rust port. Each service that had a
Python implementation *and* a Rust port fires the same request at both and
diffs the responses; a service's `WYLDE_*_IMPL` default may flip to `rust`
only once its parity test passes.

Most of those services have now been cut over and their Python halves
deleted, so their parity suites have been **retired** (see "Retired suites"
below). Two suites remain live:

| service     | impl env var          | parity test            | kind |
|-------------|-----------------------|------------------------|------|
| lifecycle   | `WYLDE_LIFECYCLE_IMPL`| `tests/lifecycle.rs`   | Python ↔ Rust diff |
| wylde-ollama| n/a (greenfield Rust) | `tests/wylde-ollama.rs`| live-Ollama smoke  |

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
   Produces `rust/target/release/wylde-{lifecycle,ollama}.exe`.

2. **Python virtualenv** (lifecycle only) — `<repo>/.venv` with the service
   dependencies installed (msgpack, pywin32, …). The suite uses
   `.venv\Scripts\python.exe` directly; the system `py -3` resolves to a
   bare interpreter without the deps.

3. **A live Ollama daemon** (ollama smoke only) — at `OLLAMA_URL`
   (default `http://127.0.0.1:11434`).

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

Run one suite:

```
cargo test --features parity --test lifecycle
WYLDE_OLLAMA_PARITY_LIVE=1 cargo test --features parity --test wylde-ollama
```

> First build is slow: as a standalone package it compiles its own copy of
> `tokio` and `wylde-shared` into `rust/tests/parity/target/`.

## Lifecycle parity

The lifecycle daemon is the long-lived supervisor a developer almost always
has running, and the Python daemon (`Core/Lifecycle/`) is still load-bearing
— the Rust `wylde-lifecycle.exe` port has **not** cut over. So this gate is
still live: it guards the eventual lifecycle flip.

Two pieces of daemon plumbing make the test possible — both shipped on the
Python *and* Rust daemon so the gate is symmetric:

1. **No-spawn mode** (`--no-spawn` / `WYLDE_LIFECYCLE_NOSPAWN=1`) — the
   control + manifest surfaces come up but `_start_<service>` forks nothing,
   recording a "would-have-spawned" entry instead. Without it a parity run
   would boot Wylde's entire `tier=core` stack. The Rust side of this
   surface lives in `rust/crates/wylde-lifecycle/src/control.rs` ("No-spawn
   parity surface"), kept byte-identical to the Python daemon's.
2. **Isolated pipe names** (`WYLDE_LIFECYCLE_PIPE_NAME`) — each parity
   daemon binds `wylde-lifecycle-parity-py` / `-rs` rather than the
   canonical `\\.\pipe\wylde-lifecycle`. This lets the test run **while a
   production lifecycle daemon is up**: the parity daemons and the live
   daemon never contend for a pipe. No-spawn mode additionally skips the
   `core.json` manifest write, so a parity daemon cannot clobber a live
   daemon's manifest either.

The 8 gated cases: `ping`, `handshake`, `lifecycle.status`,
`lifecycle.list_services`, `lifecycle.start_service`, `unknown_action`,
`empty_action`, `lifecycle.shutdown_all`. The `lifecycle.*` actions are a
no-spawn control surface both daemons answer byte-identically; the
launcher/registry-backed `service.start` / `service.list` / `service.health`
stay Python-only (the Rust port defers them) and are not gated.

Capture is **sequential** — a fixed script replayed against a fresh
no-spawn Python daemon, then a fresh no-spawn Rust daemon, then the two
reply lists diffed. A fresh process per side means each is exercised from
identical state, the fair comparison for a supervisor. It is safe to run
with the live Wylde stack up (isolated pipes).

## wylde-ollama smoke

Phase 1 of the migration is greenfield Rust — there is no Python
counterpart to diff against. The right shape here is **record/replay
against real Ollama**: spin up `wylde-ollama.exe`, fire canonical requests
through the pipe, and assert the responses match. The current file is a
minimal smoke (round-trip `ollama.health` + `ollama.list_models`); the
exhaustive per-action coverage lives in `wylde-ollama/src/actions/*`
against wiremock. Opt in with `WYLDE_OLLAMA_PARITY_LIVE=1`.

## Retired suites

When a service's Python half is deleted, its parity test is retired with it
— there is no longer a second implementation to diff against. The pattern
was set by the Phase 5.D `harness_turn.rs` retirement (PR #8, deleted when
the Python chat-turn driver disappeared). The following followed:

| suite                  | Python target (deleted)             | deletion lineage |
|------------------------|-------------------------------------|------------------|
| `tests/broker.rs`      | `Core.resource_monitor.run`         | Python broker deleted in `7072947` (test had already been `#[ignore]`d) |
| `tests/gateway.rs`     | `Gateway.run` (`Gateway/routes/*`)  | Gateway Python deleted; shim collapsed rust-only (`1b8e5bf`) |
| `tests/device_gate.rs` | `device_gate.run`                   | device_gate Python deleted `f731267` (2026-06-02) |
| `tests/vpn.rs`         | `wylde-vpn` Python                  | VPN Python deleted `9143c46` (2026-06-02) |

`tests/gateway.rs` was the sole consumer of `src/http.rs` (HTTP capture) and
of the `reqwest` dependency; both were removed with it.

## How it works

- **Lifecycle / ollama** (`tests/lifecycle.rs`, `tests/wylde-ollama.rs`) —
  sequential capture: each daemon is launched as a child process, driven
  over its named pipe via `wylde_shared::ipc::send_action`, and the reply
  envelopes are diffed (lifecycle) or asserted (ollama). The lifecycle
  daemon uses `--no-spawn` and an isolated pipe — see "Lifecycle parity".

### Gate vs. probe

A response always carries fields that *must* differ between two runs —
timestamps, UUID lease ids, pids, live hardware readings. The harness
(`src/diff.rs`) normalizes those volatile paths out of both sides before
comparing, so a diff only fires on a *real* divergence.

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
    pipe.rs             named-pipe capture
  tests/
    lifecycle.rs        lifecycle Python ↔ Rust no-spawn parity
    wylde-ollama.rs     wylde-ollama live-Ollama record/replay smoke
```
