# Manifest ownership

> ⚠️ **ARCHIVED / SUPERSEDED — describes the REMOVED Python runtime, NOT the current all-Rust stack. Kept for history.**
> The ownership *split* below (service owns its own manifest; the Lifecycle daemon supervises + sweeps orphans) is still conceptually accurate, but the mechanism is wrong: it teaches each service to call `write_manifest()`/`start_heartbeat()`/`mark_stopped()` "in its `run.py`" with `atexit.register(...)`. **No `run.py` exists anywhere** — the Python runtime was deleted in the full-Rust cutover (R6, commit `2f5aa82`, 2026-06-10). Services now publish their manifest via the Rust four-phase `ipc::serve` entry point (see `Services/wylde-images/src/main.rs` and `wylde-shared`'s manifest helpers), not a Python `run.py`. Read the table for the *ownership model*, not the API.
> *Banner added 2026-06-27 on branch `chore/structure-tidy` (structure-tidy pass).*

This is the reference for how Wylde services publish runtime state via
`data/manifests/*.json`. It became the canonical contract after the
manifest-ownership refactor that moved per-service manifest writes out
of the Lifecycle daemon and into each service's own `run.py`.

## TL;DR

| Owner | Responsibility |
| --- | --- |
| **Service** (in its `run.py`) | `write_manifest(...)` at startup, `start_heartbeat(...)` to keep `status.heartbeat` fresh, `mark_stopped(...)` from the SIGTERM/SIGINT handler. |
| **Lifecycle daemon** | Spawning + supervising service subprocesses, recording spawn intent, **and** running the orphan-detection sweep that flips abandoned-alive manifests to `dead-orphan`. |
| **Lifecycle daemon (still)** | `core.json` — the Core rollup manifest covering lifecycle / harness / memgraph / memory scheduler — because `wylde-core` is a rollup label, not a peer service. |

The daemon no longer writes per-service manifest files for Voice,
device_gate, Gateway, Memgraph, or vram-broker.

## status.state values

The `status.state` field in every manifest takes one of four values:

| Value | Set by | Meaning |
| --- | --- | --- |
| `alive` | `write_manifest()` on first call; refreshed implicitly by every heartbeat tick | Service is heartbeating. |
| `stopped` | `mark_stopped()` in the service's SIGTERM/SIGINT handler | Graceful shutdown completed. Terminal until next `write_manifest`. |
| `dead-orphan` | `mark_orphan_dead()` from Lifecycle's orphan-detection sweep | Manifest claimed `alive` but the pid was no longer running. Ungraceful exit — kill -9, segfault, OOM. Terminal until next `write_manifest`. |
| (missing) | Legacy manifests written before the state-field migration | Treated as `alive` by the sweep so the safety net still works on them. |

The schema otherwise is unchanged — `service`, `version`, `pipe`,
`port`, `category`, `description`, `contributes`, plus the `status`
block. Only `status.state`, `status.stop_time`, and `status.last_seen`
are additions.

## The canonical service startup pattern

```python
"""<service> entry point — hosts \\\\.\\pipe\\wylde-<service>."""

from __future__ import annotations

import signal
import sys
import threading

from Core.shared.logging_setup import configure_logging
from Core.shared.manifest import (
    mark_stopped,
    start_heartbeat,
    write_manifest,
)

SERVICE_NAME = "wylde-<service>"


def _serve_forever() -> int:
    configure_logging(service=SERVICE_NAME)
    write_manifest(
        service_name=SERVICE_NAME,
        port=0,                       # or the HTTP port if applicable
        category="<category>",
        description="...",
        contributes={"dashboard": {"label": "...", "icon": "...", "color": "..."}},
    )
    start_heartbeat(SERVICE_NAME)
    _install_signal_handlers()

    # ... open the pipe / start the serve loop ...

    mark_stopped(SERVICE_NAME)
    return 0


def _install_signal_handlers() -> None:
    def _handler(signum, _frame):
        mark_stopped(SERVICE_NAME)
        # ... trigger your serve loop to unwind ...

    for sig_name in ("SIGINT", "SIGTERM"):
        sig = getattr(signal, sig_name, None)
        if sig is not None:
            try:
                signal.signal(sig, _handler)
            except (ValueError, OSError):
                pass


if __name__ == "__main__":
    sys.exit(_serve_forever())
```

Key invariants (enforced by `Core/harness/dev/wylde_check` rules 18 and 19):

1. `configure_logging` happens **first**, before any manifest write, so
   the manifest / heartbeat / serve logs all flow through the same
   configuration.
2. `write_manifest` and `start_heartbeat` run before the serve loop
   starts, so the dashboard sees the service as `alive` from the
   moment it begins accepting requests.
3. The signal handler **must** call `mark_stopped(SERVICE_NAME)` so
   the manifest reflects the graceful exit. Add `atexit.register(mark_stopped, SERVICE_NAME)` belt-and-braces for paths where the signal is caught by a framework's own handler (uvicorn does this; see `Gateway/run.py`).

If `_pipe.start()` or any other helper that matches the rule's
`serve_loop` regex (`\.start\(`, `.run(`, `serve(`, etc.) is defined
**before** the orchestrator in source order, rule 18 emits a false
positive. Define the orchestrator (`main()` / `_serve_forever()`)
first; helpers come after.

## Orphan-detection contract

The Lifecycle daemon runs a background thread (interval = 60 s,
matching the unified heartbeat tick) that walks
`data/manifests/*.json`. For each manifest:

* If `status.state` is `alive` (or missing) **and** `status.pid` is no
  longer running on the host, the daemon calls `mark_orphan_dead(service_name)`.
  The manifest's `heartbeat` field is preserved so a postmortem can
  see exactly when the heartbeat thread fell silent — only `state`
  and `last_seen` change.
* `stopped` and `dead-orphan` manifests are terminal — the sweep
  skips them.

In parallel, the daemon also tracks **spawn records**: an in-memory
map of `service_name → (pid, spawn_time)` recorded by `_start_*`
functions. If a spawn record is older than `_SPAWN_GRACE_SECONDS` (30 s)
and no manifest exists on disk **and** the pid is no longer running,
the daemon logs a `failed_to_launch` warning. `_stop_*` functions
clear the record so deliberate stops don't trip this check.

Spawn records do **not** survive a daemon restart — that's deliberate.
A fresh daemon makes no claims about what previous daemons spawned;
manifests on disk (with their pids) are the cross-restart source of
truth.

## Extensions and plugins

Extensions and plugins inherit the same primitives. The canonical
pattern is identical — call `write_manifest` / `start_heartbeat` at
**activate** time, `mark_stopped` at **deactivate** time. Extensions
typically have no `run.py` because they're hosted in-process by
`Extensions/extension_bridge/dispatcher`, so the calls go in the
extension's lifecycle hooks instead of a process-entry signal handler:

```python
# Extensions/<name>/lifecycle.py

from Core.shared.manifest import mark_stopped, start_heartbeat, write_manifest

SERVICE_NAME = "wylde-extension-<name>"


def activate() -> None:
    write_manifest(
        service_name=SERVICE_NAME,
        port=0,
        category="extension",
        description="...",
        contributes={"dashboard": {"label": "...", "icon": "plug", "color": "gray"}},
    )
    start_heartbeat(SERVICE_NAME)


def deactivate() -> None:
    mark_stopped(SERVICE_NAME)
```

The Lifecycle daemon's orphan-detection sweep is the same safety net:
if the host process dies without `deactivate()` running, the
extension's manifest gets flipped to `dead-orphan` automatically.

## Files to look at

* `Core/shared/manifest.py` — the primitives (`write_manifest`,
  `start_heartbeat`, `stop_heartbeat`, `mark_stopped`, `mark_orphan_dead`,
  `update_contributes`).
* `Core/Lifecycle/daemon_state.py` — spawn records, orphan-detection
  sweep, daemon-managed `_start_*` / `_stop_*` functions.
* `Voice/run.py`, `Core/Memgraph/run.py` — concrete examples of the
  canonical pattern. (The former `device_gate/`, `Gateway/`, `VPN/`, and
  `Core/resource_monitor/` `run.py` examples were deleted once their Rust
  ports became canonical.)
* `Core/harness/dev/wylde_check.py` rules 18 (`run_py_startup_sequence`)
  and 19 (`shutdown_handler_marks_stopped`) — the lint that enforces
  this contract.
