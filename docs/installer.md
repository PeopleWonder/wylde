# Building the Wylde installer (NSIS, per-user)

This is the build-and-ship doc for the Wylde alpha desktop installer. It
turns the `wylde-gui` binary plus the backend service tree into a single
per-user `WyldeSetup-<version>.exe` that installs **without admin / UAC**.

Tooling lives under `tools/installer/`:

| File | Role |
| --- | --- |
| `wylde-installer.nsi` | The NSIS install script (sections, shortcuts, uninstaller). Driven by `/D` defines; not run by hand. |
| `build-installer.ps1` | Orchestrates build -> stage -> pack and emits the setup `.exe`. |

> Background and the longer history (Tauri bundler that preceded this,
> code-signing, the update endpoint) live in
> `Core/GUI/installer/README.md`. This doc is the operational runbook.

## Prerequisites

1. **Rust toolchain** (already present on the dev box) — to build
   `wylde-gui.exe` and the backend service binaries.
2. **NSIS** (Nullsoft Scriptable Install System) — provides `makensis.exe`,
   which compiles the `.nsi` into the setup executable.
   - The build script auto-discovers `makensis` from any of, in order:
     `-MakeNsis "<path>\makensis.exe"`, `PATH`, the **portable** location
     `%USERPROFILE%\Tools\NSIS\nsis-<ver>\makensis.exe` (newest wins), then
     the system installs at `C:\Program Files (x86)\NSIS\` / `C:\Program Files\NSIS\`.

### Portable NSIS (no-UAC build host)

`winget install NSIS.NSIS` and the `*-setup.exe` both install system-wide and
**trigger a UAC prompt** — a problem when driving the box remotely (the UAC
dialog locks the desktop until physically dismissed). The portable zip avoids
elevation entirely:

1. Grab the latest portable zip from SourceForge. The canonical files page is
   <https://sourceforge.net/projects/nsis/files/NSIS%203/> — pick the newest
   `nsis-<ver>.zip` (the `.zip`, **not** `-setup.exe`). The direct mirror URL
   carries a signed `?ts=` token, so fetch via the `.../nsis-<ver>.zip/download`
   redirect rather than hard-coding a mirror host.
2. Extract to `%USERPROFILE%\Tools\NSIS\` — yields
   `%USERPROFILE%\Tools\NSIS\nsis-<ver>\makensis.exe`. No PATH edit needed; the
   build script globs that location automatically.
3. Verify: `& "$env:USERPROFILE\Tools\NSIS\nsis-<ver>\makensis.exe" /VERSION`
   should print e.g. `v3.12`.

> Verified on this box: **NSIS 3.12 portable** at
> `C:\Users\aaron\Tools\NSIS\nsis-3.12\makensis.exe` — a full pack + per-user
> test install + uninstall round-trip passed (see the build-state memory note).

NSIS is only needed for the final **pack** step. You can stage and inspect
the install tree without it (`-StageOnly`).

## Build it

From the repo root:

```powershell
# Full build: cargo build (GUI + backend) -> stage -> pack
powershell -ExecutionPolicy Bypass -File tools\installer\build-installer.ps1

# Binaries already built (recommended on the dev box -- avoids cargo file-locks):
powershell -ExecutionPolicy Bypass -File tools\installer\build-installer.ps1 -SkipBuild

# Stage only, no NSIS required (inspect release-artifacts\stage):
powershell -ExecutionPolicy Bypass -File tools\installer\build-installer.ps1 -SkipBuild -StageOnly
```

Output: `release-artifacts\WyldeSetup-<version>.exe` (the `stage\` tree next
to it is the staging dir; both are gitignored).

> **cargo + PowerShell file-locks.** This repo has a history of "access is
> denied" flakes when `cargo` runs under PowerShell. If the build phase trips
> one, build from Git Bash instead:
> ```sh
> (cd Core/GUI && cargo build --release -p wylde-gui)
> (cd rust     && cargo build --release)
> ```
> then re-run the script with `-SkipBuild`.

## What gets bundled

The staging phase assembles the install tree from two sources:

1. **The committed repo tree** via `git archive HEAD` — the Python service
   trees (`Core/`, `Extensions/`, `N8N/`, …), `docs/`, `launch_wylde.ps1`,
   `LICENSE`, manifests, etc. `git archive` automatically omits everything
   gitignored: `.venv/`, `data/`, `logs/`, build caches, local plans.
2. **The built binaries**, overlaid exactly where `launch_wylde.ps1` looks:
   - `Core/GUI/target/release/wylde-gui.exe` — the gpui desktop app.
   - `rust/bin/wylde-*.exe` — the backend service binaries the Lifecycle
     daemon supervises (lifecycle, gateway, device-gate, vram-broker, ollama,
     harness, voice, treesitter, extension bridges). `rust/bin/` is the
     launcher's first lookup, so the Rust daemon path "just works".
   - **Excluded:** `wylde-trainer.exe` (trainer scope was cut from the alpha)
     and `wylde-voice-bench.exe` (a dev micro-benchmark).

### Not bundled, on purpose

- **ONNX models (Whisper STT / Kokoro TTS) and `onnxruntime.dll`.** These are
  hundreds of MB to multiple GB and are normally fetched into the Hugging
  Face cache on first run (`Voice/download_models.py`). The installer bundles
  `onnxruntime.dll` *only if* a build already produced one next to the voice
  binary; it never bundles the model weights. Voice is unavailable until the
  models are downloaded — by design for the alpha.
- **A Python runtime.** The default daemon is the Rust `wylde-lifecycle.exe`
  (no interpreter needed), but some supervised services (Memgraph wrapper,
  harness internals) are still Python. Provisioning the interpreter +
  `uv sync` of dependencies is a separate follow-up (see the punch-list in
  `Core/GUI/installer/README.md`); this installer lays the trees down but
  assumes Python is provisioned out of band on the alpha box.

## Install behaviour

- **Location:** `%LOCALAPPDATA%\Programs\Wylde\` (per-user, the same
  convention VS Code uses). Always user-writable.
- **No UAC.** `RequestExecutionLevel user` in the `.nsi` means Windows never
  raises an elevation prompt. (SmartScreen may still warn — the alpha is
  unsigned; see the code-signing section in `Core/GUI/installer/README.md`.)
- **Shortcuts** (Start menu always, desktop optional — both selectable on the
  Components page) launch `launch_wylde.ps1` via `powershell.exe`, **not**
  `wylde-gui.exe` directly. The launcher boots the Lifecycle daemon, waits
  for `\\.\pipe\wylde-lifecycle`, then starts the GUI. Pointing a shortcut
  straight at the bare GUI leaves the backend down and every required-service
  panel shows a stub — the exact failure
  `Core/GUI/installer/fix_desktop_shortcut.ps1` exists to undo.
> **Boundary (locked).** The installer *places files*. It does not own how the
> stack is launched or which version runs. Resolving "current" and running it
> belongs to the launcher/updater layer (`wylde-stack`, `launch_wylde.ps1`) —
> which is why the shortcut targets the launcher rather than any binary, and why
> the launcher works identically on a machine that has never seen an installer.
> See issues #92 and #97.

- **Autostart** is an *unchecked* optional component. When selected it writes
  the same daemon-first command to
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Run\Wylde`.
- **`version.txt`** is written at the install root for the self-updater and
  support tooling to read.

## How it plays with the self-updater

The in-app self-updater (`wylde-updater`, Phase 12.5) replaces the running
binary in place on the **next** launch: Windows can't overwrite a running
`.exe`, so it renames the old one aside and drops the new one in. That only
requires the install directory to be **user-writable** — which
`%LOCALAPPDATA%\Programs\Wylde\` always is. The installer therefore needs to
do nothing special for the updater beyond:

1. installing to that writable per-user location, and
2. dropping `version.txt` so the updater knows the installed version.

Re-running the installer (e.g. a fresh signed build after a key rotation) is
the supported path for major/keyed updates; routine micro-fixes self-update.

## Uninstall

- **Settings -> Apps -> Wylde -> Uninstall**, or run
  `%LOCALAPPDATA%\Programs\Wylde\uninstall.exe`.
- The uninstaller is registered per-user under
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Uninstall\Wylde` (no admin
  to list or remove).
- It removes the Start-menu and desktop shortcuts, the autostart `Run` value,
  the per-user registry keys, and the entire install directory.
- Runtime state the app wrote elsewhere (HF model cache, any
  `C:\ProgramData\Wylde\` scheduler state) is intentionally left behind so a
  reinstall doesn't have to re-download models.

## Testing without touching a working dev install

The install root (`%LOCALAPPDATA%\Programs\Wylde\`) is distinct from a
source checkout, so a test install will not overwrite a developer's repo. If
you still want to keep a prior packaged install untouched, install into a
throwaway dir on the Directory page (e.g. `%LOCALAPPDATA%\Wylde-test`) and
uninstall it afterwards — the uninstaller only ever touches its own
`$INSTDIR` and the per-user keys it wrote.
