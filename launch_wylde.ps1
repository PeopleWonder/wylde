# launch_wylde.ps1 -- desktop shortcut entry point.
#
# Boots the Lifecycle daemon (which spawns Memgraph + Voice + Device
# Gate as subprocesses, and starts the harness pipe in-process), waits
# for \\.\pipe\wylde-lifecycle to come up, then launches the gpui GUI
# (`wylde-gui.exe`).
#
# Slice 11 (final cutover, 2026-05-29) replaced the Tauri+Svelte GUI
# with the gpui-native `wylde-gui` binary built out of the standalone
# `Core/GUI/` workspace. The old `src-tauri/target/release/*.exe`
# lookup + `npm run tauri dev` fallback are gone with that tree.
#
# Logs to $env:TEMP\wylde-launch.log so the desktop shortcut can be
# debugged without keeping a console window open.

$ErrorActionPreference = 'Stop'
$WyldeRoot   = '%USERPROFILE%\Documents\Obsidian Vault\Wylde'
$WyldeParent = Split-Path -Parent $WyldeRoot
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

# --- PYTHONPATH ----------------------------------------------------
# The daemon's _start_<service> overlays use parent-of-Wylde so
# children resolve "from Wylde.X import Y". Mirror that here for the
# daemon process itself.
if ($env:PYTHONPATH) {
    $env:PYTHONPATH = "$WyldeParent;$env:PYTHONPATH"
} else {
    $env:PYTHONPATH = $WyldeParent
}
Log "PYTHONPATH: $env:PYTHONPATH"

# --- Pick Rust vs Python daemon (strangler-fig) -------------------
# WYLDE_LIFECYCLE_IMPL switches the entire daemon between
# implementations. R4 ported the Lifecycle daemon to Rust; both
# daemons read the same on-disk manifest schema (wire-compatible per
# wylde_shared::manifest's parity tests), so a live cutover stays
# consistent regardless of which side is running.
#
# Defaults to 'rust' (2026-05-30 flip): the Wylde user's `py -3` resolves to
# Python 3.14, which doesn't have the project's .venv deps, so the
# Python daemon can fail to bind \\.\pipe\wylde-lifecycle and leave the
# GUI's Chat unreachable. The Rust binary has no interpreter dependency.
#
# Resolution order:
#   1. WYLDE_LIFECYCLE_IMPL=python  -> force Python (manual override)
#   2. otherwise                    -> Rust if the binary exists
#   3. Rust binary missing          -> fall back to Python with a warning
$ImplOverride = if ($env:WYLDE_LIFECYCLE_IMPL) { $env:WYLDE_LIFECYCLE_IMPL.ToLower() } else { '' }
if ($ImplOverride -and $ImplOverride -ne 'python' -and $ImplOverride -ne 'rust') {
    Log "WARN: WYLDE_LIFECYCLE_IMPL=$ImplOverride is not 'python' or 'rust'; ignoring (using default 'rust')"
    $ImplOverride = ''
}

# Rust binary resolution mirrors _services.py::_rust_binary_path:
# bundled rust/bin first, then release, then debug.
$RustBinCandidates = @(
    (Join-Path $WyldeRoot 'rust\bin\wylde-lifecycle.exe'),
    (Join-Path $WyldeRoot 'rust\target\release\wylde-lifecycle.exe'),
    (Join-Path $WyldeRoot 'rust\target\debug\wylde-lifecycle.exe')
)
$RustBin = $RustBinCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1

if ($ImplOverride -eq 'python') {
    $DaemonImpl = 'python'
    Log "daemon impl: python (explicit WYLDE_LIFECYCLE_IMPL=python override)"
} elseif ($RustBin) {
    $DaemonImpl = 'rust'
    if ($ImplOverride -eq 'rust') {
        Log "daemon impl: rust (explicit WYLDE_LIFECYCLE_IMPL=rust)"
    } else {
        Log "daemon impl: rust (default)"
    }
} else {
    # Default is rust, but the binary isn't built. Don't hard-fail --
    # fall back to the Python daemon so the user still gets a stack,
    # but make the reason loud so future maintainers knows why he's on the
    # fragile py-3.14 path.
    $DaemonImpl = 'python'
    Log "WARN: no wylde-lifecycle.exe found; falling back to Python daemon (py -3 -m Core.Lifecycle.daemon)"
    Log "  searched: $($RustBinCandidates -join ', ')"
    Log "  build the Rust daemon with: cargo build --release -p wylde-lifecycle"
    Write-Warning "Rust lifecycle binary missing -- falling back to fragile Python daemon. Build it: cargo build --release -p wylde-lifecycle (see $LogPath)"
}

if ($DaemonImpl -eq 'rust') {
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
} else {
    # --- Boot the Python daemon (detached, windowless) ------------
    # Start-Process -WindowStyle Hidden gives us a detached process
    # that survives launcher exit; the daemon's internal Popen calls
    # (Memgraph, Voice, Device Gate) inherit the env we built above.
    Log "spawning Python Lifecycle daemon"
    try {
        $daemonProc = Start-Process `
            -FilePath 'powershell.exe' `
            -ArgumentList @('-NoProfile', '-Command', 'py -3 -m Core.Lifecycle.daemon') `
            -WorkingDirectory $WyldeRoot `
            -WindowStyle Hidden `
            -PassThru
        Log "daemon spawned: pid=$($daemonProc.Id)"
    } catch {
        Log "ERROR: daemon spawn failed: $_"
        Write-Error "Failed to start Wylde daemon -- see $LogPath"
        exit 1
    }
}

# --- Wait for the lifecycle pipe ----------------------------------
# Get-ChildItem \\.\pipe\ lists every named pipe on the system; we
# poll for up to 20 s waiting for wylde-lifecycle to appear. The
# pipe shows up as soon as Core.shared.ipc.serve_forever_background
# binds it, well before the rest of phase 2 finishes.
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
