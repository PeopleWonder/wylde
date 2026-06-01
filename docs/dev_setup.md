# Dev setup

## Syncing the venv

Always use the `--all-extras` flag:

```
uv sync --all-extras
```

Run it:

- after pulling new commits
- after switching branches
- after any edit to `pyproject.toml` or `uv.lock`

### Why `--all-extras`

`pyproject.toml` declares optional extras (`harness`, `trainer`, etc.) that several services import unconditionally at runtime. A plain `uv sync` installs only the base `[project] dependencies` and prunes anything from a previous extras-enabled sync that is no longer required, so services that depend on extras break.

Hard deps in `[project] dependencies` (e.g. `passlib`) survive a plain `uv sync` — that has been verified empirically. If a core dep appears to "disappear", the venv almost certainly isn't torn; see the next section.

## Venv health check

After a branch switch, a `uv sync`, or any time an import looks suspicious, run:

```
uv run python verification/check_venv.py
```

The script imports every name in `[project] dependencies` and verifies the running interpreter actually lives inside `.venv/`. Exits 0 + prints `ok (…)` on success; exits 1 with a per-package report otherwise.

One-liner equivalent for a quick smoke test:

```
uv run python -c "import fastapi, uvicorn, pydantic, pydantic_settings, httpx, requests, yaml, msgpack, win32api, passlib; print('ok')"
```

## The "passlib keeps disappearing" pattern

This was diagnosed and the root cause is **not** that `uv` removes passlib. It does not — plain `uv sync` keeps everything in `[project] dependencies`, including `passlib`.

The real cause is **interpreter confusion**: `py -3` on this machine resolves to a system **Python 3.14** install that has no project deps installed. Any script invoked via `py -3 …` (notably the `launch_wylde.ps1` shim that runs `py -3 -m Core.Lifecycle.daemon`) raises `ModuleNotFoundError: No module named 'passlib'`. The error text is identical to a genuinely missing dep, so it gets mis-diagnosed as a torn venv. A `uv sync --all-extras` then "fixes" it by populating the venv afresh — but the actual passlib in `.venv\Lib\site-packages\` was never gone, and the next run via `py -3` repeats the symptom.

Three things to know:

1. **Always invoke Python through `uv run` or `.venv\Scripts\python.exe`.** Never `py`, never `python`, never `python3`. Those resolve via `PATH` and on this box jump to Python 3.14, which is *not* the venv.
2. **When in doubt, run `verification/check_venv.py`.** The first failure it reports is "interpreter is NOT the project venv" — that diagnosis takes five seconds and rules out the common case.
3. **The launcher (`launch_wylde.ps1`) uses `py -3`.** That works today only because the user has a second system Python 3.11 install with most deps `pip install`-ed into it. It is fragile by design — a future drift between system 3.11 and the venv will look like packages "vanishing". Long-term fix: launcher should call `.venv\Scripts\python.exe` directly.

## When something is missing

If `verification/check_venv.py` reports a real missing import (not the interpreter mismatch):

1. `uv sync --all-extras` — re-run the canonical sync.
2. Confirm the dep is in `pyproject.toml` under `[project] dependencies` (hard dep) or `[project.optional-dependencies]` (extra).
3. Confirm the dep is locked in `uv.lock`.
4. If the dep is locked but missing from `.venv\Lib\site-packages\`, the venv is torn — recreate with `uv venv --clear` then `uv sync --all-extras`.
