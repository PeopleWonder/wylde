# Strict mypy mode — full-monorepo rollout completion report

> ⚠️ **ARCHIVED / STALE — describes the REMOVED Python runtime, NOT the current all-Rust stack. Kept for history.**
> The "full-monorepo strict mypy" walk this report describes covered the Python services that were **deleted in the full-Rust cutover (R6, commit `2f5aa82`, 2026-06-10)**. The only Python remaining is the dev-time `wylde_check` architecture linter under `Core/harness/dev/`; there is no longer a monorepo-wide Python codebase to type-check. Kept for historical reference.
> *Banner added 2026-06-27 on branch `chore/structure-tidy` (structure-tidy pass).*

**Status:** complete. Every service is on strict mypy. Unified mypy walk over the
monorepo returns a single error — and that error is in
`Core/harness/dev/wylde_check/rules/_actions.py:359`, which is owned by a
parallel rule-additions task that the rollout was explicitly forbidden from
touching.

## Default mypy invocation going forward

```bash
uv run mypy --explicit-package-bases Core Gateway device_gate Voice VPN Trainer N8N Extensions
```

This walks 418 source files. Expected steady-state output: **0 errors** (1
during the wylde_check parallel work).

## Per-service results

| Service | Pre-flip errors | Post-flip errors | Notes |
|---|---:|---:|---|
| device_gate | 0 | 0 | Already strict-clean before rollout |
| resource_monitor (`Core.resource_monitor.*`) | 36 | 0 | Override pattern was previously `resource_monitor.*` which did not match — fixed to `Core.resource_monitor.*`. That alone surfaced the 36 errors below. |
| extension_bridge (`Extensions.extension_bridge.*`) | 2 | 0 | Same override-pattern issue (`extension_bridge.*` → `Extensions.extension_bridge.*`). |
| Memgraph (`Core.Memgraph.*`) | 39 | 0 | Override was active but annotations had never been done. Mostly Flask route handlers + an unimported `Any` in `graph_service.py`. |
| Voice | 2 → 47 once strict flipped | 0 | Production code + 3 test files retrofitted. |
| VPN | 7 → 37 once strict flipped | 0 | Production code + signal handler + Flask handlers. |
| Trainer | 0 → 12 once strict flipped | 0 | All annotations; no behavior changes. |
| Gateway | 2 → 54 once strict flipped | 0 | Middleware `dispatch` signatures + two test files. |
| Extensions/Webcrawler | 7 → 3 once strict flipped | 0 | Trivial `dict[str, Any]` annotation in `extractor.py` + smoke-test fixes. |
| Extensions/Wylde_Study | 0 → 2 once strict flipped | 0 | Two lazy-import helpers. |
| Core (`Core.*`, the residual after the above) | 564 once strict flipped | **1 (out-of-scope wylde_check rule)** | See breakdown below. |

### Core breakdown (564 → 1)

- `Core/Lifecycle/` — 22 errors → 0. Production code in `daemon_state/__init__.py`, two test files (`test_orphan_sweep.py`, `test_shutdown_all.py`). Also removed a vestigial `from Wylde.Core.Lifecycle import …` fallback in `test_shutdown_all.py` because strict mode treated it as a redef of the primary import.
- `Core/shared/` — 171 → 0. Two pre-existing non-strict errors fixed in the process: `discovery.py:430` union-attr now narrowed via `assert isinstance(listener, _CatalogListener)`; `ipc.py:1284` TestClient/Flask union now routed through a typed-`Any` shadow.
- `Core/harness/` — 449 → 1. The single residual is the out-of-scope wylde_check error.

## Latent bugs caught

**None confirmed.** Both delegated agents (Core/shared, Core/harness) reported
zero real bugs; every error was an annotation gap. This contrasts with the N8N
flip (which surfaced 7 broken-since-written tool wrappers per the prior memory)
and is the right outcome — the architectural refactors that landed in advance
of this work (manifest ownership, split monolithic files, daemon split) already
shook out the latent bugs.

The two `signal` handlers in run.py files (resource_monitor, Voice, VPN, Memgraph,
Gateway) all needed `(int, FrameType | None) -> None` signatures. None of them
had wrong behavior, but their unannotated state would have hidden any future
typo (e.g. swapping `signum` with `_frame`).

## Architectural decisions surfaced

**None.** Two small consistency notes:

1. `VPN/peers/store.py`, `VPN/peers/push.py`, `Gateway/secrets/vault_backend.py`,
   `Gateway/egress/destinations.py`, `Extensions/Webcrawler/tests/smoke_test.py`,
   `Extensions/extension_bridge/tests/smoke_test.py` all had the same pattern:
   ```python
   def _load() -> dict:
       return json.loads(path.read_text())
   ```
   which mypy flagged with `no-any-return` because `json.loads` returns `Any`.
   The fix throughout was a typed local binding:
   ```python
   data: dict = json.loads(path.read_text())
   return data
   ```
   No casts, no `# type: ignore`. Worth noting as the canonical pattern.

2. `_PipePool._pools` in `Core/Memgraph/ipc.py` was using non-parameterized
   `queue.LifoQueue` which gave Any-typed reads. Parameterizing as
   `queue.LifoQueue[_PipeHandle]` fixed the `no-any-return` cascade in
   `acquire(...)`. Same pattern works in `Core/shared/ipc.py`.

## Constraint compliance

- **No new `# type: ignore`.** All existing ones (Memgraph/run.py, VPN/run.py for
  the manifest-import fallback noops, the wylde_check carve-out) date back to
  before this rollout.
- **wylde_check rule code untouched.** The single remaining error is in
  `Core/harness/dev/wylde_check/rules/_actions.py:359` — `get_docstring` arg
  type — which is the parallel rule-additions task's domain.
- **Manifest refactor preserved.** Signal handlers in every `run.py` now have
  proper `(int, FrameType | None) -> None` signatures. `mark_stopped` lazy
  imports use `*args: object, **kwargs: object` (Memgraph) or
  `*args: Any, **kwargs: Any` (Voice/VPN) in the import-fallback no-ops; no
  call sites changed.
- **No file splits undone.** The broker, wylde_check, pipe, and daemon_state
  splits are intact — the override patterns now correctly target the dotted
  module paths after the splits.

## Verification

- `uv run mypy --explicit-package-bases Core Gateway device_gate Voice VPN Trainer N8N Extensions` → **1 error** (the out-of-scope wylde_check rule).
- `uv run pytest Core Gateway device_gate Voice VPN Trainer N8N Extensions` → **486 passed, 0 failed** (vs. ≥442 baseline).
- `uv run ruff check .` → **All checks passed!**
- `wylde_check.run_all()` → **0 findings**.

## Pyproject changes

The `[[tool.mypy.overrides]]` block list now ends with `module = ["Core.*"]`,
which is the umbrella that catches everything in the `Core/` tree. The
sub-overrides (`Core.Memgraph.*`, `Core.resource_monitor.*`) are technically
redundant once `Core.*` is in place, but they stay for documentation — each
landed at a different phase of the rollout and reading the file top-to-bottom
preserves the history.
