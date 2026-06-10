# launch_wylde.ps1 -- desktop shortcut entry point.
#
# Boots the Rust Lifecycle daemon (`wylde-lifecycle.exe`, which spawns
# every service as a subprocess), waits for \\.\pipe\wylde-lifecycle to
# come up, then launches the gpui GUI (`wylde-gui.exe`).
#
# Full-Rust cutover (R6, 2026-06-10): the Python Lifecycle daemon and
# every other Python runtime service were deleted, so the strangler-fig
# WYLDE_LIFECYCLE_IMPL switch and the PYTHONPATH overlay are gone too.
# The stack is rust-only: no interpreter, no .venv, no fallback.
#
# Slice 11 (final cutover, 2026-05-29) replaced the Tauri+Svelte GUI
# with the gpui-native `wylde-gui` binary built out of the standalone
# `Core/GUI/` workspace.
#
# Logs to $env:TEMP\wylde-launch.log so the desktop shortcut can be
# debugged without keeping a console window open.

$ErrorActionPreference = 'Stop'
$WyldeRoot   = $PSScriptRoot
$LogPath     = Join-Path $env:TEMP 'wylde-launch.log'

function Log {
    param([string]$Message)
    $stamp = Get-Date -Format 'yyyy-MM-dd HH:mm:ss'
    Add-Content -Path $LogPath -Value "[$stamp] $Message" -Encoding utf8
}

# Reset the log on each launch so the user only ever sees the most
# recent run; old runs would mostly be noise.
Set-Content -Path $LogPath -Value "" -Encoding utf8
Log "launcher: starting"
Log "wylde root: $WyldeRoot"

# --- Resolve the Rust Lifecycle daemon -----------------------------
# Bundled rust/bin first, then release, then debug (mirrors
# services.rs::rust_binary_path).
$RustBinCandidates = @(
    (Join-Path $WyldeRoot 'rust\bin\wylde-lifecycle.exe'),
    (Join-Path $WyldeRoot 'rust\target\release\wylde-lifecycle.exe'),
    (Join-Path $WyldeRoot 'rust\target\debug\wylde-lifecycle.exe')
)
$RustBin = $RustBinCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1

if (-not $RustBin) {
    # Rust-only stack: there is no Python daemon to fall back to.
    Log "ERROR: no wylde-lifecycle.exe found"
    Log "  searched: $($RustBinCandidates -join ', ')"
    Log "  build with: cargo build --release -p wylde-lifecycle"
    Write-Error "Wylde daemon binary missing -- build it with `cargo build --release -p wylde-lifecycle` (see $LogPath)"
    exit 1
}

if ($env:WYLDE_LIFECYCLE_IMPL -and $env:WYLDE_LIFECYCLE_IMPL.ToLower() -ne 'rust') {
    Log "WARN: WYLDE_LIFECYCLE_IMPL=$($env:WYLDE_LIFECYCLE_IMPL) ignored -- the Python daemon was removed in the full-Rust cutover; the stack is rust-only"
}

Log "spawning Rust Lifecycle daemon: $RustBin"
try {
    $daemonProc = Start-Process `
        -FilePath $RustBin `
        -WorkingDirectory $WyldeRoot `
        -WindowStyle Hidden `
        -PassThru
    Log "daemon spawned: pid=$($daemonProc.Id)"
} catch {
    Log "ERROR: rust daemon spawn failed: $_"
    Write-Error "Failed to start Wylde daemon -- see $LogPath"
    exit 1
}

# --- Wait for the lifecycle pipe ----------------------------------
# Get-ChildItem \\.\pipe\ lists every named pipe on the system; we
# poll for up to 20 s waiting for wylde-lifecycle to appear. The pipe
# shows up as soon as the daemon binds it, well before the rest of the
# boot sequence finishes.
$pipeReady = $false
$deadline = (Get-Date).AddSeconds(20)
while ((Get-Date) -lt $deadline) {
    try {
        $pipes = Get-ChildItem '\\.\pipe\' -ErrorAction Stop |
                 Select-Object -ExpandProperty Name
        if ($pipes -contains 'wylde-lifecycle') {
            $pipeReady = $true
            break
        }
    } catch {
        # Pipe directory enumeration sometimes flakes on first call;
        # swallow and retry.
    }
    Start-Sleep -Milliseconds 250
}

if (-not $pipeReady) {
    Log "ERROR: \\.\pipe\wylde-lifecycle did not come up within 20s"
    if ($daemonProc -and -not $daemonProc.HasExited) {
        Log "killing orphaned daemon (pid=$($daemonProc.Id))"
        try { Stop-Process -Id $daemonProc.Id -Force -ErrorAction SilentlyContinue } catch {}
    }
    Write-Error "Wylde daemon failed to bring the lifecycle pipe up -- see $LogPath"
    exit 1
}
Log "lifecycle pipe up -- daemon ready"

# Best-effort: log the pipes the daemon actually exposed. Useful when
# the GUI later complains about a missing service.
try {
    $allPipes = Get-ChildItem '\\.\pipe\' | Select-Object -ExpandProperty Name |
                Where-Object { $_ -like 'wylde-*' }
    Log ("wylde pipes: " + ($allPipes -join ', '))
} catch {}

# --- Launch the gpui GUI ------------------------------------------
# `wylde-gui` builds out of the standalone Core/GUI/ workspace, so its
# binary lands under Core/GUI/target/{release,debug}/wylde-gui.exe
# (NOT rust/target/ -- the gpui workspace is deliberately separate from
# the backend workspace so gpui's heavy graphics deps don't ripple into
# the backend lock file). Prefer release; fall back to debug.
$GuiRoot = Join-Path $WyldeRoot 'Core\GUI'
$GuiBinCandidates = @(
    (Join-Path $GuiRoot 'target\release\wylde-gui.exe'),
    (Join-Path $GuiRoot 'target\debug\wylde-gui.exe')
)
$GuiBin = $GuiBinCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1

if (-not $GuiBin) {
    Log "ERROR: no wylde-gui.exe found"
    Log "  searched: $($GuiBinCandidates -join ', ')"
    Log "  build with: cargo build --release -p wylde-gui   (from Core/GUI/)"
    Write-Error "Wylde GUI binary missing -- see $LogPath"
    exit 1
}

Log "launching gpui GUI: $GuiBin"
try {
    Start-Process -FilePath $GuiBin -WorkingDirectory $GuiRoot
} catch {
    Log "ERROR: GUI launch failed: $_"
    Write-Error "Failed to launch Wylde GUI -- see $LogPath"
    exit 1
}

Log "launcher: done -- GUI process spawned"
