# Phase 5 cutover — chat-turn driver (2026-05-25)

Slice 5.D of the Wylde Rust migration. Closes Phase 5 of
[wylde-rust-migration-master-plan.md](wylde-rust-migration-master-plan.md).

## What flipped

The two strangler-fig env vars governing the chat-turn surface now
default to `rust`:

| Knob                              | Consumer                         | Old default | New default |
|-----------------------------------|----------------------------------|-------------|-------------|
| `WYLDE_WYLDE_HARNESS_IMPL`        | Lifecycle daemon spawn decision  | `python`    | `rust`      |
| `WYLDE_HARNESS_IMPL`              | Python `_chat.py` forwarder      | `python`    | `rust`      |
| `WYLDE_HARNESS_MODELS_IMPL`       | Python `_models.py` forwarder **and** Rust `models.*` handler gate (`actions.rs::rust_enabled`) | `python` | `rust` |

> **Slice 3b addendum (2026-06-03).** `WYLDE_HARNESS_MODELS_IMPL` now
> defaults to `rust`. It governs **both** halves of the `models.*`
> strangler: the Python `_models.py` entry points forward eight verbs
> (`list`, `get_profile`, `show`, `delete`, `unload`, `set_active`,
> `set_default`, `get_default`) over the harness pipe, and the Rust
> handlers run live unless the flag is an explicit `python` (the rollback
> path, where they return `not_implemented` and the Python body takes
> over). `models.transcribe` / `models.synthesize` stay Python-only — they
> drive the Voice STT/TTS engines, which aren't hosted in the harness
> crate, so there's no Rust handler to forward to. A self-loop guard in
> `_models.py` suppresses the forward when the Python harness is itself the
> live pipe server (the env var is decoupled from the daemon's
> service-selection flag, so the `python`-server + `rust`-models
> misconfiguration is reachable). Set `WYLDE_HARNESS_MODELS_IMPL=python`
> to revert.

Effect at boot:

1. Lifecycle daemon (Python or Rust) spawns `wylde-harness.exe`
   alongside the existing Python `wylde-harness` service. Both bind
   `\\.\pipe\wylde-harness`; the Rust binary wins because it comes up
   first under the post-flip topology.
2. The Python harness pipe's `_chat.py` action handler reads
   `WYLDE_HARNESS_IMPL` (default `rust`) and forwards `chat.run_turn`
   to the Rust pipe. Transport faults still fall through to the
   in-process Python driver, so a missing binary or a daemon mis-spawn
   can't take the chat brain offline.

`WYLDE_WYLDE_HARNESS_IMPL=python` reverts everything — the daemon
skips the Rust spawn and the forwarder stops forwarding. The legacy
`WYLDE_HARNESS_TURN_IMPL` name is still honoured as a one-release
fallback (the 2026-05-24 consolidation rename).

## Parity gate

> **RETIRED 2026-06-03 (deletion slice).** `harness_turn.rs` diffed the
> Rust salvage parser against Python's `Core.harness.turn._streaming` via
> a `.venv` subprocess probe. When the Python driver package was deleted
> (below) there was no Python half left to diff against, so the test and
> the parity crate's `wylde-harness` dependency were removed. The Rust
> salvage parser keeps its own coverage in `wylde-harness`'s lib tests.
> The description below is retained as the historical record of what the
> gate covered at cutover.

`rust/tests/parity/tests/harness_turn.rs` (new in 5.D). Three test
functions covering the pure-function port surface byte-for-byte
against Python's `Core.harness.turn._streaming`:

| Test                            | Cases | Coverage                                                              |
|---------------------------------|-------|-----------------------------------------------------------------------|
| `salvage_parity`                | 15    | bare / tag-wrapped / fenced JSON + alias resolution + prose guard     |
| `call_hash_parity`              | 5     | empty / scalar / nested / ASCII-escape / order-insensitive dedupe key |
| `find_balanced_braces_parity`   | 5     | balanced / escaped quotes / unbalanced trailer / empty input          |

Run: `cd rust/tests/parity && cargo test --features parity --test harness_turn -- --nocapture`

All 25 cases at parity on the first run. The pre-existing per-side
test suites (Rust: `crates/wylde-harness/tests/run_turn_loop_e2e.rs`;
Python: `Core/harness/tests/test_turn/*`) cover the turn-loop control
flow and `tool_calls_summary` shape on each side; we relied on
those for end-to-end coverage rather than building a cross-language
stub-Ollama scaffold.

## Why pure-function parity rather than full chat.run_turn parity

Symmetric end-to-end parity would need a deterministic stub Ollama
on a shared isolated pipe driving both implementations sequentially
— substantial new test infrastructure for marginal coverage, because
the dispatch-loop control flow is stable framework-level behaviour
that both impls were unit-tested for at port time. The pieces that
drift silently are byte-level: salvage-parser regex semantics,
balanced-brace scanner escape handling, call_hash canonicalisation.
Those are exactly what the new file gates, and their parity is the
load-bearing prerequisite for the dispatch loop to produce the same
`tool_calls_summary` rows.

## Python deletion punchlist

The Python driver at `Core/harness/turn/` stays on disk for one
release cycle as the rollback path. **Earliest deletion: 2026-06-08**
(14-day soak from cutover).

> **EXECUTED 2026-06-03** (ahead of the 2026-06-08 soak date, once the
> prereqs in PR #6 closed the last two couplings — all unary `chat.*`
> verbs forwarding to Rust, and the non-driver helpers rehomed to
> `Core/harness/_tool_context.py`). `Core/harness/turn/` (7 files,
> 2,369 LOC) and the driver-only `test_turn/` suite (7 files, 1,500 LOC)
> were deleted; `_chat.py` lost its strangler scaffolding and the three
> unary handlers became thin Rust forwarders that raise
> `harness_unavailable` rather than fall back (no Python loop remains);
> `test_strangler_fig.py` moved to `tests/test_chat_forwarder.py` as a
> Rust-forwarder-only suite; the `harness_turn.rs` parity gate was
> retired (see above). Rust serves all five `chat.*` verbs, verified live
> over the real pipe. The `WYLDE_HARNESS_IMPL` / `WYLDE_HARNESS_TURN_IMPL`
> rollback knob is gone with the driver it gated.

### Paths to delete

* `Core/harness/turn/` — entire package (`__init__.py`, `_driver.py`,
  `_end_of_turn.py`, `_request_build.py`, `_state.py`, `_streaming.py`,
  `_tool_round.py`).
* `Core/harness/tests/test_turn/` — every file except
  `test_strangler_fig.py`, which becomes "Rust forwarder only" and
  moves to `Core/harness/tests/`.
* `Core/harness/pipe/_chat.py` — strip the strangler-fig fallback
  scaffolding (`_harness_turn_impl`, `_try_forward_run_turn_to_rust`)
  along with the `_turn` import and the in-process call path. The
  five chat.* handlers become thin IPC forwarders to
  `\\.\pipe\wylde-harness`.

### Pre-deletion verification

Before running the delete:

1. Confirm `WYLDE_HARNESS_IMPL=python` produces no measurable
   production traffic — grep daemon logs for the
   "`WYLDE_WYLDE_HARNESS_IMPL=python`" line for the 14-day window;
   none should appear.
2. Re-run `cargo test --features parity --test harness_turn`. The
   15 + 5 + 5 cases must still be at parity.
3. Re-run `pytest Core/harness/tests/`. Every test that exercises
   `Core.harness.turn._driver.run_turn` must either be deleted (it
   covered the Python driver) or rewritten to exercise the Rust
   forwarder via the harness pipe.
4. Smoke `chat.run_turn` via the production pipe with `model=gemma3:4b`
   and `user_message="say hi"`. The reply's `turn_id` must be a
   32-char hex (Rust shape, no hyphens — the existing tell).
5. Walk Phase 7 / 8 / 9 imports for any lingering
   `from Core.harness.turn import …` references. They become Phase 6's
   tooling registry calls or Phase 7's memory layer calls — neither
   should reach into the chat-turn module post-deletion.

### Rollback if the soak surfaces a divergence

Per-machine: `setx WYLDE_WYLDE_HARNESS_IMPL python` + tray restart.
Repo-wide: revert this slice's edits to `_chat.py`,
`_services_harness.py`, `services.rs`, `daemon.rs`, and the
`HARNESS` doc-comment in `state/mod.rs`. The Python driver is still
on disk (deletion deferred), so reverting only the defaults restores
the pre-5.D topology.

## Files changed in this slice

* `Core/harness/pipe/_chat.py` — default flipped to `rust` in
  `_harness_turn_impl()`; module + function docstrings updated.
* `Core/harness/tests/test_turn/test_strangler_fig.py` — three
  default-asserting test names updated to reflect the new default
  + an explicit-`python` rollback test added.
* `Core/Lifecycle/daemon_state/__init__.py` — comment block for
  `_harness_proc` updated.
* `Core/Lifecycle/daemon_state/_services_harness.py` —
  `_impl_for("wylde-harness")` calls now pass `default="rust"`;
  module + function docstrings updated.
* `rust/crates/wylde-lifecycle/src/state/services.rs` —
  `start_harness` reads `impl_for_with_default(…, ImplLang::Rust)`;
  banner comment updated.
* `rust/crates/wylde-lifecycle/src/state/mod.rs` — `HARNESS`
  doc-comment rewritten to reflect the new default.
* `rust/crates/wylde-lifecycle/src/daemon.rs` — `start_harness`
  comment updated.
* `docs/wylde-rust-migration-master-plan.md` — Phase 5 row marked
  CUTOVER + next-action set to the deletion date.
* `rust/tests/parity/Cargo.toml` — `wylde-harness` added as a
  parity dep (consumes the salvage parser in-process).
* `rust/tests/parity/tests/harness_turn.rs` — NEW. 25 byte-level
  parity cases across the salvage parser, call_hash, and
  find_balanced_braces.

## Gates run

| Gate                                            | Result                                              |
|-------------------------------------------------|-----------------------------------------------------|
| `cargo test -p wylde-harness`                   | ✅ 148/148                                          |
| `cargo test -p wylde-lifecycle`                 | ✅ 55/55                                            |
| `cargo test --features parity --test harness_turn` | ✅ 3/3 functions (15 + 5 + 5 cases at parity)    |
| `cargo check --workspace --all-targets`         | ✅ clean                                            |
| `cargo clippy --workspace --all-targets`        | ⚠ 11 pre-existing warnings in Phase 7.A workspace memory (`memory/workspaces/`) — not from this slice |
| `mypy` on changed files                          | ✅ clean                                            |
| `pytest Core/harness/tests/`                    | ✅ 145/145                                          |
| `pytest Core/Lifecycle/tests/`                   | ✅ passed (count rolled into above)                |
| `cargo build --release --workspace`             | ❌ blocked by running stack — wylde-memgraph (pid 36440) + wylde-voice (pid 36088) hold fresh manifests. Pre-build guard correctly refuses. Unblock by stopping those services or the whole stack (`tray → Shut down`). |

## Open punchlist surfaced during this slice

1. **call_hash non-ASCII divergence** — Python `json.dumps` defaults
   to `ensure_ascii=True` (escapes non-ASCII to `\uXXXX`); Rust
   `serde_json` does not. The dedupe set is process-local so this
   doesn't break production, but it's a port-fidelity gap worth
   closing under Phase 8 hygiene. Reproducer: any tool call whose
   args contain non-ASCII string content will produce different
   hex digests on either side.
2. **Pre-existing clippy noise in `wylde-harness/src/memory/workspaces/actions.rs`** — 10 `await_holding_lock`
   warnings in test code, plus 1 `unused_imports` in `store.rs`.
   Phase 7.A landed earlier today; flag for the 7.A owner to
   chase. Not blocking this cutover (none in 5.D files).
3. **Stale manifest GC vs prebuild guard** — `wylde-memgraph` /
   `wylde-voice` python.exe processes can have their manifest
   heartbeats fresh enough (< 300s) to block a release rebuild even
   when the operator has no other wylde-* binaries to lock. The
   guard is correctly conservative here, but if this pattern keeps
   recurring it might be worth distinguishing
   "manifest-heartbeat-fresh from a python.exe" from "binary actually
   running" — only the latter holds a file lock on the .exe target.

## Related memory

* [[wylde-phase5-slice-5a-shipped]] — slice 5.A foundation (the
  standalone-crate era, since consolidated).
* [[wylde-phase6-shipped]] — Phase 6 (tooling) shipped on the same
  day; the alias map this slice's salvage parser uses is registry-
  populated post-Phase 6.
