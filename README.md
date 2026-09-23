# Wylde

**Wylde is a local-first personal AI system.** It runs entirely on your own
machine: a native desktop app, a local LLM runtime, on-device voice, private
long-term memory, and an optional encrypted tunnel so you can reach it from
your phone — with no third-party cloud in the request path.

> **Public alpha.** Wylde is under active development. Expect rough edges,
> breaking changes between builds, and features that are still landing. It is
> not yet hardened for untrusted multi-user or internet-exposed deployment —
> see [Security model](#security-model).

## What it is

- **All Rust.** The services, the desktop GUI, and the chat-turn engine are
  native Rust — the full-Rust cutover (2026-06-10) deleted the last Python
  runtime services and their rollback paths. The only Python left in-tree is
  the `wylde_check` lint tool (`Core/harness/dev/`) and the stdlib N8N
  extension tool stubs; nothing at runtime needs an interpreter.
- **Native desktop GUI.** The app is `wylde-gui`, a GPU-rendered
  [gpui](https://github.com/zed-industries/zed/tree/main/crates/gpui) (Rust)
  desktop application under `Core/GUI/`. There is no web stack, no embedded
  browser SPA, and no HTTP for local traffic — panels talk to the backend over
  named pipes directly from Rust. (This replaced an earlier Tauri + Svelte
  alpha at the 2026-05 cutover.)
- **Local models.** Chat runs against a local [Ollama](https://ollama.com)
  daemon; voice runs Whisper (STT) and Kokoro (TTS) on-device, with optional
  NPU acceleration.
- **Private by default.** All outbound internet access funnels through a single
  audited egress channel with a kill switch. Remote access from your phone is
  opt-in and rides an encrypted WireGuard tunnel.

## Architecture

Wylde is a set of small services, each owning one concern and exposing a Windows
named pipe (`\\.\pipe\wylde-*`). The desktop GUI and other in-process callers
speak those pipes directly; the Gateway is the only HTTP trust boundary.

| Service              | Pipe                        | Role |
| -------------------- | --------------------------- | ---- |
| **harness**          | `\\.\pipe\wylde-harness`    | The "chat brain": the chat-turn loop, the tool catalog + runner, the model registry, and the three memory layers (long-term, workspace, short-term) with RAG. Rust (`wylde-harness`). |
| **gateway**          | `\\.\pipe\wylde-gateway` + HTTP `127.0.0.1:8005` | The single HTTP trust boundary: outbound egress (allowlist + kill switch), inbound mobile-over-VPN, and the MCP surface. Two auth tiers (public / local). Rust (`wylde-gateway`). |
| **lifecycle**        | `\\.\pipe\wylde-lifecycle`  | Daemon supervisor: spawns, heartbeats, and gracefully shuts down every other service from each service's `manifest.json`. Rust (`wylde-lifecycle`). |
| **voice**            | `\\.\pipe\wylde-voice`      | Audio I/O + the capture→STT→chat→TTS loop, cpal mic, openWakeWord. Rust (`wylde-voice`). |
| **vram-broker**      | `\\.\pipe\vram-broker`      | Lease-based VRAM arbitration so models cooperate on a single GPU, with DRAM spillover. Rust (`wylde-vram-broker`). |
| **ollama**           | `\\.\pipe\wylde-ollama`     | Wraps the local Ollama HTTP API behind the lease primitive. Rust (`wylde-ollama`). |
| **memgraph**         | `\\.\pipe\wylde-memgraph`   | Knowledge-graph store. The harness talks to Neo4j over **direct Bolt** (`bolt://127.0.0.1:7687`); the lifecycle daemon supervises the bundled Neo4j JVM directly. |
| **device-gate**      | `\\.\pipe\wylde-device-gate`| Per-device bearer tokens + permission tiers (`read_only` / `tool_use` / `destructive_tool_access`) for paired mobile devices. Rust (`wylde-device-gate`). |
| **extension-bridge** | `\\.\pipe\wylde-extension-bridge` | Hosts browser extensions and the MCP tool surface; routes per-extension manifests. Rust (`wylde-extension-bridge`). |
| **vpn**              | `\\.\pipe\wylde-vpn`        | WyldeLink mesh: WireGuard wrapper + mDNS discovery + phone pairing. Opt-in (default-off). |

The desktop GUI (`wylde-gui`) is its own Cargo workspace under `Core/GUI/`,
deliberately separate from the backend `rust/` workspace so gpui's graphics
dependencies don't ripple the backend lockfile. It renders ten first-party
panels (Chat, Workspaces, Tools, Memory, Models, Dashboard, Devices,
RemoteAccess, Images, Settings), each a gpui `View` crate, plus
extension-contributed iframe panels (e.g. the n8n editor) hosted in a `wry`
WebView child window.

`WYLDE_ENDPOINTS.md` is the exhaustive endpoint-and-pathway reference;
`docs/wylde-repo-organization.md` is the repo orientation map.

## Repository layout

| Path | Contents |
| ---- | -------- |
| `rust/`        | The backend Cargo workspace — every Rust service crate (`wylde-harness`, `wylde-gateway`, `wylde-lifecycle`, `wylde-voice`, `wylde-vram-broker`, `wylde-ollama`, `wylde-device-gate`, `wylde-extension-bridge`, `wylde-vpn`) plus `wylde-shared`. |
| `Core/GUI/`    | The native gpui desktop app (separate workspace). Ships `wylde-gui`. |
| `Core/`        | Non-service assets beside the GUI workspace: `Core/Memgraph/vendor/` (bundled Neo4j), `Core/harness/dev/` (the `wylde_check` lint tool + tests — the only Python left), service `manifest.json` files. The Python runtime services that used to live here (`Lifecycle/`, `harness/`, `shared/`, the Memgraph wrapper) were deleted in the full-Rust cutover (R6, 2026-06-10). |
| `N8N/`, `Extensions/` | Extension manifests, workflow templates, and stdlib tool stubs. |
| `docs/`        | Architecture, design, extension, and migration docs (see `docs/wylde-repo-organization.md` for the index). |

## Building and running

> **New here? Follow [docs/setup.md](docs/setup.md)** for step-by-step
> clone-to-running instructions (under 10 minutes). The sections below are
> the reference detail behind that guide.

### Install (Windows)

> **There is no working packaged installer.** The `WyldeSetup-<version>.exe`
> asset on the Releases page does **not** produce a working install and never
> has. Do not use it. The NSIS installer work has moved to
> [PeopleWonder/wylde-installer](https://github.com/PeopleWonder/wylde-installer),
> where it is parked as planned future work.

The only supported route is a **development checkout** — build from source and
launch via `launch_wylde.ps1`. See [docs/setup.md](docs/setup.md).

### Prerequisites

- **Rust** (stable toolchain) for the services and the GUI.
- **[Ollama](https://ollama.com)** running locally for chat (`127.0.0.1:11434`).
- **Python via [uv](https://docs.astral.sh/uv/)** — optional, dev-only: the
  `wylde_check` lint suite is the only thing that needs an interpreter.
- **Neo4j + JDK runtime.** Not shipped in the repo (~280 MB, above GitHub's
  file-size limit). Run `tools\install-neo4j.ps1` once to download pinned,
  checksum-verified versions into `Core/Memgraph/vendor/` — see
  [docs/setup.md](docs/setup.md).

### Run it

The supported entry point is the launcher, which brings up the lifecycle daemon
(daemon-first) and then the GUI. **The GUI never spawns services itself.**

```powershell
.\launch_wylde.ps1
```

This is what the desktop shortcut should point at. Launching the bare
`wylde-gui.exe` without the daemon results in "no services / missing panels".

### Build from source

Backend services (run from the repo root):

```powershell
cargo build --release --manifest-path rust/Cargo.toml
```

Desktop GUI (separate workspace — building the backend does **not** build the
GUI and vice-versa):

```powershell
cargo build --release --manifest-path Core/GUI/Cargo.toml
# produces Core/GUI/target/release/wylde-gui.exe (the entry_point in Core/GUI/manifest.json)
```

### Python (dev-only, optional)

The runtime needs no interpreter. The one Python tool left is `wylde_check`
(`Core/harness/dev/` — the repo's 48-rule lint suite); to hack on it, install
[uv](https://docs.astral.sh/uv/) and run `uv venv && uv sync --extra dev`
(pytest, ruff, mypy — there are no runtime dependencies, and the old
service extras went with their Python packages in the full-Rust cutover).

### Tests

```powershell
cargo test --manifest-path rust/Cargo.toml          # backend Rust
cargo test --manifest-path Core/GUI/Cargo.toml      # GUI
uv run pytest Core N8N                              # wylde_check lint-tool suite
```

## Security model

- **Single egress channel.** Internal components reach the public internet only
  through the Gateway's `/api/egress/*` surface, which enforces a per-component
  allowlist and a global kill switch.
- **Auth lives at the tunnel.** The Gateway has exactly two tiers
  (public / local); remote-device authentication happens at the WyldeLink VPN
  boundary, not as per-route API keys. Remote access is opt-in and default-off.
- **Per-device permission tiers.** Paired phones get a bearer token at the
  `read_only` tier by default; promoting to `tool_use` or
  `destructive_tool_access` is an explicit action.
- **Local-only by default.** With the VPN off, every service binds to loopback
  and the only HTTP listener is the Gateway on `127.0.0.1:8005`.

This is alpha software — do not expose it directly to the public internet.

## License

Wylde is licensed under the GNU General Public License v3.0 or later. See [LICENSE](./LICENSE) for the full text.
