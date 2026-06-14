# wylde-dev.ps1 -- the "Wylde Dev" hot-iteration environment.
#
# FULL-STACK hot-reload: both halves of the stack rebuild on save.
#
#   GUI half  (bacon dev, foreground): save a GUI .rs -> app killed,
#             incrementally rebuilt, relaunched; save visual_style_v1.yaml
#             -> live theme repaint (no rebuild).
#   Backend half (wylde-backend-watch.ps1, its own window): save a backend
#             service crate's .rs -> that ONE service rebuilds and bounces
#             via the dev daemon's dev.restart_service verb; the GUI and
#             every other service stay up.
#
# What it does, in order:
#   1. Stages the backend dev binaries + sets the dev daemon's environment
#      (WYLDE_DEV_HOTRELOAD gate + per-service WYLDE_<NAME>_BIN overrides ->
#      rust/target-dev/stage). The daemon spawns each service from its
#      STAGED copy, so the backend watcher can rebuild into
#      rust/target-dev/debug/ WITHOUT fighting the running .exe's lock, then
#      ask the daemon to swap+respawn.
#   2. Boots the Rust Lifecycle daemon IF the pipe isn't already up -- now a
#      *dev* daemon (it inherits the gate + overrides from step 1). If a
#      stack is already up, probes whether it's a dev daemon: if yes, reuse
#      + hot-reload; if it's a plain stack, backend hot-reload is disabled
#      for the session (GUI hot-reload still works) with a hint to relaunch
#      from a clean state. Fail-soft throughout (OI-1 banners).
#   3. Sets the dev-only GUI acceleration + hot-reload environment:
#        CARGO_TARGET_DIR = Core/GUI/target-dev   (separate GUI cache)
#        RUSTFLAGS        = -C linker=rust-lld     (fast dev linker)
#        WYLDE_THEME_PATH = .../visual_style_v1.yaml  (live theme reload)
#      All scoped to this script + target-dev caches -- the shipped release
#      build (rust/target, Core/GUI/target) is never touched. See the
#      dev-env report for why the linker is NOT in .cargo/config.toml.
#   4. Launches the backend watcher (its own window) when a dev daemon is
#      confirmed, then hands off to `bacon dev` (the GUI loop) in the
#      foreground. Quitting bacon (q) tears the backend watcher down.
#
# Invoke as:  powershell -NoProfile -ExecutionPolicy Bypass -File wylde-dev.ps1
# (the "Wylde Dev" desktop shortcut does exactly that). No elevation.
#
#   -NoBackendHotReload   GUI-only loop (the pre-2026-06-13 behaviour).
#
# First run stages from already-built binaries (fast) or, for any service
# with no binary yet, builds it cold into target-dev once.

param(
    [switch]$NoBackendHotReload
)

$ErrorActionPreference = 'Stop'
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$GuiRoot  = Join-Path $RepoRoot 'Core\GUI'
$RustRoot = Join-Path $RepoRoot 'rust'
$BackendWatch = Join-Path $PSScriptRoot 'wylde-backend-watch.ps1'

Write-Host "Wylde Dev -- repo: $RepoRoot"

# Daemon-managed backend services (1:1 crate==service==bin). Mirrors
# DAEMON_MANAGED_SERVICES in rust/crates/wylde-lifecycle/src/control.rs,
# minus wylde-memgraph (Neo4j JVM, not a rebuildable Rust binary).
$DaemonServices = @(
    'wylde-workspaces', 'wylde-harness', 'wylde-gateway', 'wylde-ollama',
    'wylde-vram-broker', 'wylde-device-gate', 'wylde-extension-bridge',
    'wylde-voice', 'wylde-vpn', 'wylde-treesitter', 'wylde-n8n'
)
# Stage-only (not daemon-supervised; GUI-spawned).
$StageOnlyServices = @('wylde-lsp')

$HotReload = -not $NoBackendHotReload
$StageDir  = Join-Path $RustRoot 'target-dev\stage'
$DevDebug  = Join-Path $RustRoot 'target-dev\debug'

function Bin-EnvName([string]$svc) {
    'WYLDE_' + ($svc.ToUpper() -replace '-', '_') + '_BIN'
}

# Write a UTF-8 file with NO byte-order mark. Windows PowerShell 5.1's
# `Set-Content -Encoding utf8` prepends a BOM (EF BB BF); ipc_call reads the
# JSON payload byte-for-byte and rejects a leading BOM as "payload is not valid
# JSON: expected value at line 1 column 1". This helper guarantees BOM-free
# bytes on every PowerShell edition.
function Write-Utf8NoBom([string]$path, [string]$text) {
    [System.IO.File]::WriteAllText($path, $text, (New-Object System.Text.UTF8Encoding($false)))
}

# ── 1. Stage backend binaries + set the dev daemon environment ─────────
if ($HotReload) {
    Write-Host ""
    Write-Host "staging backend dev binaries (rust/target-dev/stage) ..."
    New-Item -ItemType Directory -Force -Path $StageDir | Out-Null

    $allSvc = $DaemonServices + $StageOnlyServices
    $needBuild = @()
    foreach ($svc in $allSvc) {
        $stage = Join-Path $StageDir "$svc.exe"
        if (Test-Path $stage) { continue }
        # Seed from the best already-built binary (fast path).
        $src = @(
            (Join-Path $RustRoot "bin\$svc.exe"),
            (Join-Path $RustRoot "target\release\$svc.exe"),
            (Join-Path $RustRoot "target\debug\$svc.exe"),
            (Join-Path $DevDebug "$svc.exe")
        ) | Where-Object { Test-Path $_ } | Select-Object -First 1
        if ($src) { Copy-Item $src $stage -Force }
        else { $needBuild += $svc }
    }

    # Any service with no binary anywhere: build it once into target-dev
    # (cold first-run cost, one cargo invocation for all of them).
    if ($needBuild.Count -gt 0) {
        Write-Host "  building (cold) into target-dev: $($needBuild -join ', ')"
        $prevTarget = $env:CARGO_TARGET_DIR; $prevFlags = $env:RUSTFLAGS
        $env:CARGO_TARGET_DIR = Join-Path $RustRoot 'target-dev'
        $env:RUSTFLAGS = '-C linker=rust-lld'
        $pkgArgs = @(); foreach ($s in $needBuild) { $pkgArgs += @('-p', $s) }
        Push-Location $RustRoot
        # NB: no `2>&1`. cargo logs progress to stderr; under this script's
        # ErrorActionPreference='Stop', merging a native command's stderr into
        # the pipeline (2>&1) wraps each line in a terminating NativeCommandError
        # in PowerShell 5.1 -- it would crash the launcher on the first
        # "Compiling ..." line. Letting stderr flow straight to the console is
        # safe and still shows the build output.
        & cargo build @pkgArgs | Out-Host
        Pop-Location
        $env:CARGO_TARGET_DIR = $prevTarget; $env:RUSTFLAGS = $prevFlags
        foreach ($svc in $needBuild) {
            $built = Join-Path $DevDebug "$svc.exe"
            if (Test-Path $built) { Copy-Item $built (Join-Path $StageDir "$svc.exe") -Force }
            else { Write-Warning "  $svc has no binary yet -- it will be built+staged on first edit." }
        }
    }

    # Point the daemon at the staged binaries + open the dev gate. Only set
    # an override when the stage file actually exists (a SET-but-missing
    # WYLDE_<NAME>_BIN would otherwise keep that service dark all session).
    $env:WYLDE_DEV_HOTRELOAD = '1'
    $env:WYLDE_ROOT = $RepoRoot
    $staged = 0
    foreach ($svc in $allSvc) {
        $stage = Join-Path $StageDir "$svc.exe"
        if (Test-Path $stage) {
            Set-Item -Path ("env:" + (Bin-EnvName $svc)) -Value $stage
            $staged++
        }
    }
    Write-Host "staged $staged service binary(ies); dev gate WYLDE_DEV_HOTRELOAD=1"
}

# ── helper: is a lifecycle daemon already up, and is it a dev daemon? ───
function Test-PipeUp {
    try {
        return ([System.IO.Directory]::GetFiles('\\.\pipe\') |
            Where-Object { $_ -like '*wylde-lifecycle' }).Count -gt 0
    } catch { return $false }
}

# Build the dev ipc client once (lib crate -> links while the stack is up).
function Ensure-IpcCall {
    $ipc = Join-Path $DevDebug 'examples\ipc_call.exe'
    if (Test-Path $ipc) { return $ipc }
    $prevTarget = $env:CARGO_TARGET_DIR; $prevFlags = $env:RUSTFLAGS
    $env:CARGO_TARGET_DIR = Join-Path $RustRoot 'target-dev'
    $env:RUSTFLAGS = '-C linker=rust-lld'
    Push-Location $RustRoot
    # No `2>&1` -- see the note in step 1: merging cargo's stderr under
    # ErrorActionPreference='Stop' would crash the launcher in PS 5.1.
    & cargo build -q -p wylde-shared --example ipc_call | Out-Host
    Pop-Location
    $env:CARGO_TARGET_DIR = $prevTarget; $env:RUSTFLAGS = $prevFlags
    if (Test-Path $ipc) { return $ipc } else { return $null }
}

# Probe whether the running daemon exposes dev.restart_service. An
# unregistered action returns error.code == 'no_action' (plain daemon);
# a dev daemon returns a different code (not_registered for the bogus name).
function Test-DevDaemon([string]$ipc) {
    if (-not $ipc) { return $false }
    $pf = Join-Path $env:TEMP 'wylde-devprobe.json'
    # BOM-free (see Write-Utf8NoBom) -- a BOM here makes ipc_call reject the
    # probe as invalid JSON and write to stderr, which used to crash the whole
    # launcher (BOM -> native stderr -> terminating error). And capture stdout
    # WITHOUT `2>&1`: any stderr from ipc_call must not be merged into the
    # pipeline under ErrorActionPreference='Stop'. The structured reply we test
    # below comes on stdout regardless.
    Write-Utf8NoBom $pf '{"name":"__probe__"}'
    $out = & $ipc lifecycle dev.restart_service "@$pf"
    Remove-Item $pf -ErrorAction SilentlyContinue
    try {
        $j = ($out | Out-String | ConvertFrom-Json)
        if ($j.error -and $j.error.code -eq 'no_action') { return $false }
        return $true
    } catch { return $false }
}

# ── 2. Lifecycle daemon (boot dev daemon, or detect a reused stack) ────
$pipeUp = Test-PipeUp
$devDaemon = $false

if (-not $pipeUp) {
    $daemonCandidates = @(
        (Join-Path $RepoRoot 'rust\bin\wylde-lifecycle.exe'),
        (Join-Path $RepoRoot 'rust\target\release\wylde-lifecycle.exe'),
        (Join-Path $RepoRoot 'rust\target\debug\wylde-lifecycle.exe')
    )
    $daemon = $daemonCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1
    if ($daemon) {
        Write-Host "starting lifecycle daemon: $daemon"
        # Inherits WYLDE_DEV_HOTRELOAD + WYLDE_<NAME>_BIN from step 1 -> a dev daemon.
        Start-Process -FilePath $daemon -WorkingDirectory $RepoRoot -WindowStyle Hidden | Out-Null
        $deadline = (Get-Date).AddSeconds(20)
        while ((Get-Date) -lt $deadline) {
            if (Test-PipeUp) { $pipeUp = $true; break }
            Start-Sleep -Milliseconds 250
        }
        if ($pipeUp -and $HotReload) { $devDaemon = $true }
    }
    if (-not $pipeUp) {
        Write-Warning "lifecycle pipe not up -- the dev GUI will run with degraded services (OI-1 banners)."
    }
} else {
    Write-Host "lifecycle pipe already up -- reusing the running stack"
    if ($HotReload) {
        $ipc = Ensure-IpcCall
        $devDaemon = Test-DevDaemon $ipc
        if (-not $devDaemon) {
            Write-Warning ("a NON-dev stack is already running. Backend hot-reload needs the dev daemon, " +
                "so it is DISABLED this session (GUI hot-reload still works). For full-stack hot-reload, " +
                "stop the stack (Dashboard -> Shut down, or close Wylde) and relaunch Wylde Dev.")
        }
    }
}

# ── 3. Dev-only GUI environment ────────────────────────────────────────
$env:CARGO_TARGET_DIR = Join-Path $GuiRoot 'target-dev'
$env:RUSTFLAGS        = '-C linker=rust-lld'
$env:WYLDE_THEME_PATH = Join-Path $GuiRoot 'Frontend\Panels\Workspaces\assets\visual_style_v1.yaml'

Write-Host ""
Write-Host "target dir : $env:CARGO_TARGET_DIR (GUI)"
Write-Host "linker     : rust-lld (dev only)"
Write-Host "theme hot  : $env:WYLDE_THEME_PATH"

# ── 4. Launch the backend watcher (own window), then the GUI loop ──────
$watcher = $null
if ($HotReload -and $devDaemon) {
    Write-Host "backend hot-reload: ON -- launching watcher window"
    $watcher = Start-Process powershell `
        -ArgumentList '-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $BackendWatch, '-RepoRoot', $RepoRoot `
        -PassThru -WindowStyle Normal
} elseif ($HotReload) {
    Write-Host "backend hot-reload: OFF (no dev daemon) -- GUI hot-reload only"
} else {
    Write-Host "backend hot-reload: OFF (-NoBackendHotReload) -- GUI hot-reload only"
}

Write-Host ""
Write-Host "bacon dev -- save a GUI .rs to rebuild+relaunch; edit the YAML to restyle live; q quits."
Set-Location $GuiRoot
try {
    bacon dev
} finally {
    if ($watcher -and -not $watcher.HasExited) {
        Write-Host "stopping backend watcher (pid $($watcher.Id))"
        Stop-Process -Id $watcher.Id -Force -ErrorAction SilentlyContinue
    }
}
