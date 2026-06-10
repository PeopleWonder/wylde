# Dev setup

> **Full-Rust cutover (R6, 2026-06-10):** the runtime needs no Python.
> Everything below concerns the OPTIONAL dev venv for the `wylde_check`
> lint tool (`Core/harness/dev/`) — the only Python left in-tree.

## Syncing the venv (optional, dev-only)

```
uv venv
uv sync --extra dev    # pytest, ruff, mypy — there are no runtime deps
```

Run the lint-tool suite with:

```
uv run pytest Core N8N
```

## Interpreter discipline (historical, still good advice)

Always invoke Python through `uv run` or `.venv\Scripts\python.exe` —
never `py`, `python`, or `python3` from `PATH`. On this machine `py -3`
resolves to a system Python 3.14 with no project deps, which historically
produced `ModuleNotFoundError`s that looked exactly like a torn venv.
(The launcher no longer runs any Python, so this now only matters when
running `wylde_check` or its tests by hand.)

The historical torn-venv investigation lives in git history of this file
(pre-R6) if a future dep mystery needs it.
