# Wylde — Setup

Clone → running in under 10 minutes on Windows. If anything is unclear, the
[Troubleshooting](#troubleshooting) section at the bottom covers the common
snags.

## 1. Prerequisites

- **Windows 10/11** (Linux/macOS not yet supported — the launcher, services,
  and the bundled Neo4j runtime are Windows-only for now).
- **Rust toolchain** (stable) — https://rustup.rs
- **Ollama** (local LLM runtime, listens on `127.0.0.1:11434`) — https://ollama.com/download
- **Python via [uv](https://docs.astral.sh/uv/)** — for the Python-supervised
  pieces (bundled Neo4j supervision). Install once:
  ```powershell
  powershell -ExecutionPolicy Bypass -c "irm https://astral.sh/uv/install.ps1 | iex"
  ```
- **PowerShell 5.1+** (built into Windows).

## 2. Install the vendored runtime (Neo4j + JDK)

The repo does **not** include Neo4j or its JDK (~280 MB combined, above
GitHub's 100 MB per-file limit). The installer downloads pinned,
checksum-verified versions on first run:

```powershell
cd <repo-root>
powershell -ExecutionPolicy Bypass -File tools\install-neo4j.ps1
```

It lays everything out exactly where the launcher expects:

| Component                  | Version      | Source (checksum-verified)                    |
| -------------------------- | ------------ | --------------------------------------------- |
| Eclipse Temurin JDK        | `21.0.10+7`  | Adoptium GitHub release                        |
| Neo4j Community Edition    | `2026.03.1`  | `dist.neo4j.org`                               |
| Graph Data Science plugin  | `2026.03.0`  | bundled by the Neo4j distribution (`products/`); else GitHub release |

→ `Core/Memgraph/vendor/jdk/` and `Core/Memgraph/vendor/neo4j/`.

~3 minutes on a decent connection. **Idempotent** — safe to re-run; it reuses
a cached, checksum-valid download and exits early if everything is already in
place. Pass `-Force` to reinstall from scratch. It bundles its own JDK and
never touches your system `PATH`, so an existing Java install won't interfere.

## 3. Python dependencies

From the repo root:

```powershell
uv venv                  # creates .venv (Python 3.11, per .python-version)
uv sync --extra memgraph # Neo4j driver; add other extras as needed (see README)
```

## 4. Pull a chat model (Ollama)

Make sure the Ollama service is running, then pull whatever model you want
Wylde's chat to use, e.g.:

```powershell
ollama pull llama3.1
```

## 5. Build

The backend services and the GUI are **separate** Cargo workspaces — building
one does not build the other:

```powershell
cargo build --release --manifest-path rust\Cargo.toml      # backend services
cargo build --release --manifest-path Core\GUI\Cargo.toml  # desktop GUI (wylde-gui.exe)
```

## 6. Run

The supported entry point is the launcher, which brings up the lifecycle daemon
first and then the GUI (the GUI never spawns services itself):

```powershell
powershell -ExecutionPolicy Bypass -File launch_wylde.ps1
```

Optionally install a desktop shortcut that points at the launcher:

```powershell
powershell -ExecutionPolicy Bypass -File tools\install-desktop-shortcut.ps1
```

## Troubleshooting

- **`running scripts is disabled on this system`** — prefix each command with
  `powershell -ExecutionPolicy Bypass -File <script>` (as shown above), or set
  the policy for your user once: `Set-ExecutionPolicy -Scope CurrentUser RemoteSigned`.
- **Installer reports a checksum mismatch** — the download was corrupted or
  interrupted. Delete `Core\Memgraph\vendor\.download-cache\` and re-run
  `tools\install-neo4j.ps1`.
- **"no services / missing panels" in the GUI** — you launched `wylde-gui.exe`
  directly. Always start via `launch_wylde.ps1` so the daemon comes up first.
- **Chat does nothing / connection errors** — Ollama isn't running or has no
  model pulled. Confirm `127.0.0.1:11434` responds and you've run `ollama pull`.
- **Neo4j won't start** — confirm `Core\Memgraph\vendor\jdk\bin\java.exe` and
  `Core\Memgraph\vendor\neo4j\bin\neo4j.bat` exist (re-run the installer with
  `-Force` if not). Neo4j logs go to
  `%USERPROFILE%\Documents\default\core\wylde-memgraph\logs\`.
- **Verify your environment** — `tools\preflight-function-test.ps1` runs a
  battery of checks against a running install.

See the top-level [README](../README.md) for architecture, the full list of
`uv` extras, the security model, and the test commands.
