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

## Dev graph database (bundled Neo4j) — needs JDK 21

The graph layer (`memory/memgraph`) talks Bolt to a **vendored Neo4j
Community** at `Core/Memgraph/vendor/neo4j` (v2026.03.1). That build is
compiled for **Java 21** (`Build-Jdk-Spec: 21`, class-file major 65) — it
will not launch on JDK 17 (`UnsupportedClassVersionError`). Docker is
**not** used for this; run the bundled JVM-hosted Neo4j directly.

### One-time: get a JDK 21 without disturbing the system JDK 17

Other tooling on this box pins JDK 17 (`JAVA_HOME` →
`~/dev-tools/jdk-17.x`), so install JDK 21 **side-by-side** as a portable
zip (no elevation, no global `JAVA_HOME` change):

```bash
# Temurin/Adoptium 21 (matches the existing ~/dev-tools portable pattern)
curl -sL -o ~/dev-tools/jdk21.zip \
  "https://github.com/adoptium/temurin21-binaries/releases/download/jdk-21.0.11%2B10/OpenJDK21U-jdk_x64_windows_hotspot_21.0.11_10.zip"
cd ~/dev-tools && unzip -q -o jdk21.zip          # -> ~/dev-tools/jdk-21.0.11+10
```

winget also works if you prefer a managed install:
`winget install Microsoft.OpenJDK.21` (or `EclipseAdoptium.Temurin.21.JDK`).

### Launch Neo4j (JAVA_HOME scoped to this shell only)

```bash
cd "Core/Memgraph/vendor/neo4j"
export JAVA_HOME="$HOME/dev-tools/jdk-21.0.11+10"   # scoped — don't rewrite global JAVA_HOME
./bin/neo4j-admin.bat server console                # Ctrl-C to stop
```

Confirm it's listening: `netstat -ano | grep 127.0.0.1:7687` should show
`LISTENING`. Auth is disabled in the vendored `conf/neo4j.conf`, so no
credentials are needed. (`cypher-shell.bat` honours `JAVACMD`, not
`JAVA_HOME` — set `export JAVACMD="$HOME/dev-tools/jdk-21.0.11+10/bin/java.exe"`
if you invoke it directly.)

### Run the live graph tests

With Neo4j up, exercise the Bolt layer end-to-end:

```bash
cargo test -p wylde-harness --test memgraph_live -- --ignored --nocapture
```

These are `#[ignore]`d (they need a live DB) and cover upsert /
traverse / multihop / relate / unrelate / upsert_edge / stats /
delete_workspace and the `memory.workspace.save` → graph write. Point
them elsewhere with `GRAPH_BOLT_URL` / `GRAPH_USER` / `GRAPH_PASSWORD`.
