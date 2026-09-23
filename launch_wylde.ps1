# launch_wylde.ps1 -- THE fixed, version-independent entry point.
#
# Boots the Rust Lifecycle daemon (`wylde-lifecycle.exe`, which spawns
# every service as a subprocess), waits for \\.\pipe\wylde-lifecycle to
# come up, then launches the gpui GUI (`wylde-gui.exe`).
#
# ---------------------------------------------------------------------
# Resolution (issue #92)
# ---------------------------------------------------------------------
# This script used to resolve each binary itself, taking the FIRST hit
# across rust\bin -> rust\target\release -> rust\target\debug. That had
# two failure modes and both bit:
#
#   * Shadowing -- one stale artifact at an earlier candidate silently
#     won over a fresh build forever. Rebuild, still launch the old one.
#   * Profile mixing -- because the walk ran per binary, a single launch
#     could take the daemon from target\release and a service from
#     rust\bin, which have no version relationship to each other.
#
# Resolution now lives in ONE place -- the `wylde-stack` crate -- which
# the self-updater also uses, so the launch path and the update path
# cannot drift apart. This script asks `wylde-stack.exe resolve --json`
# and runs exactly what it is told. `wylde-stack` picks a single
# directory for the whole stack: the `current` pointer the updater
# maintains (%LOCALAPPDATA%\Wylde\current) when one exists, otherwise the
# build tree.
#
# The build-tree fallback preserves the OLD candidate order for the
# daemon, so a dev machine with no pointer resolves the daemon to exactly
# the file it always did -- what changed is that every other binary now
# follows the daemon's directory instead of restarting the walk.
#
# ---------------------------------------------------------------------
# Bootstrap fallback -- deliberate, do not remove
# ---------------------------------------------------------------------
# `wylde-stack.exe` is itself a Wylde binary, so a tree that predates it
# (or a checkout that hasn't been rebuilt since #97/#92 landed) will not
# have one. Rather than refuse to launch, this script falls back to the
# legacy inline resolution and logs a loud warning. Launching is more
# important than launching optimally: bricking the only way to start
# Wylde would be a far worse bug than the staleness this fixes.
#
# Logs to $env:TEMP\wylde-launch.log so the desktop shortcut can be
# debugged without keeping a console window open.

$ErrorActionPreference = 'Stop'
$WyldeRoot   = $PSScriptRoot
# Export WYLDE_ROOT so the Rust Lifecycle daemon -- and every service it
# spawns as a subprocess -- resolves the estate root from this env var
# rather than from the process working directory (the `.` fallback in
# wylde-lifecycle/src/paths.rs). This makes the whole stack independent of
# where it was launched from, and a future estate move becomes a one-line
# env-var update instead of a code/config hunt. Mirrors tools/dev/wylde-dev.ps1.
$env:WYLDE_ROOT = $WyldeRoot
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

# --- Locate the resolver -------------------------------------------
# Bootstrap only. Everything AFTER this point comes from the resolver,
# so this is the one place the script still probes candidates itself --
# and it probes for a single binary, not for the stack.
$StackBinCandidates = @(
    (Join-Path $WyldeRoot 'rust\bin\wylde-stack.exe'),
    (Join-Path $WyldeRoot 'rust\target\release\wylde-stack.exe'),
    (Join-Path $WyldeRoot 'rust\target\debug\wylde-stack.exe')
)
$StackBin = $StackBinCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1

$RustBin = $null
$GuiBin  = $null

if ($StackBin) {
    # Everything about invoking the resolver is inside try/catch, and the
    # WHOLE block is best-effort: any failure here must fall through to the
    # legacy resolution below rather than stop the launch. Failing to launch
    # would be a far worse bug than the staleness this fixes.
    #
    # $ErrorActionPreference is relaxed to 'Continue' across the native
    # call, and stderr goes to a FILE. Both are load-bearing. In Windows
    # PowerShell 5.1 ANY redirection of a native exe's stderr wraps each
    # line in an ErrorRecord, and under $ErrorActionPreference='Stop' (set
    # at the top of this script) that is a TERMINATING NativeCommandError
    # EVEN WHEN THE EXE EXITED 0. `wylde-stack` writes a "not built: ..."
    # warning to stderr on any partially-built tree -- the normal state of a
    # dev machine -- so leaving the preference at 'Stop' here would abort
    # the launcher on nearly every launch, silently, because the shortcut
    # runs with no window. Verified empirically, not assumed.
    Log "resolver: $StackBin"
    $errFile = Join-Path $env:TEMP 'wylde-stack-resolve.err'
    $prevEAP = $ErrorActionPreference
    try {
        $ErrorActionPreference = 'Continue'
        $jsonText = (& $StackBin resolve --json 2>$errFile | Out-String)
        $resolveExit = $LASTEXITCODE
        $ErrorActionPreference = $prevEAP

        if (Test-Path $errFile) {
            Get-Content $errFile -ErrorAction SilentlyContinue |
                Where-Object { $_ -and $_.Trim() } |
                ForEach-Object { Log "resolver: $_" }
        }

        if ($resolveExit -eq 0) {
            $stack = $jsonText | ConvertFrom-Json
            Log ("resolved from: " + ($stack.source | ConvertTo-Json -Compress))
            foreach ($b in $stack.binaries) {
                if ($b.path) { Log ("  {0,-28} {1}" -f $b.name, $b.path) }
                else         { Log ("  {0,-28} <not built>" -f $b.name) }
            }
            $RustBin = ($stack.binaries | Where-Object { $_.name -eq 'wylde-lifecycle' }).path
            $GuiBin  = ($stack.binaries | Where-Object { $_.name -eq 'wylde-gui' }).path
        } else {
            Log "WARN: resolver exited $resolveExit; falling back to legacy resolution"
        }
    } catch {
        Log "WARN: resolver failed ($_); falling back to legacy resolution"
        $RustBin = $null
        $GuiBin  = $null
    } finally {
        # Restore the strict preference no matter which path we left by --
        # the rest of the script relies on it.
        $ErrorActionPreference = $prevEAP
        Remove-Item $errFile -Force -ErrorAction SilentlyContinue
    }
} else {
    Log "WARN: no wylde-stack.exe found -- this tree predates the shared resolver (#92)"
    Log "  build it with: cargo build --release -p wylde-stack   (from rust/)"
    Log "  falling back to legacy per-binary resolution for this launch"
}

# --- Legacy fallback ------------------------------------------------
# Only reached when the resolver is absent or unusable. This is the OLD
# first-match-across-profiles walk, with its shadowing hazard intact --
# it exists so a tree without wylde-stack.exe can still start, not
# because it is correct. Rebuilding restores proper resolution.
if (-not $RustBin) {
    $RustBinCandidates = @(
        (Join-Path $WyldeRoot 'rust\bin\wylde-lifecycle.exe'),
        (Join-Path $WyldeRoot 'rust\target\release\wylde-lifecycle.exe'),
        (Join-Path $WyldeRoot 'rust\target\debug\wylde-lifecycle.exe')
    )
    $RustBin = $RustBinCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1
    if ($RustBin) { Log "legacy resolution picked daemon: $RustBin" }
}

if (-not $RustBin) {
    # Rust-only stack: there is no Python daemon to fall back to.
    Log "ERROR: no wylde-lifecycle.exe found"
    Log "  build with: cargo build --release -p wylde-lifecycle"
    Write-Error "Wylde daemon binary missing -- build it with ``cargo build --release -p wylde-lifecycle`` (see $LogPath)"
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
# `wylde-gui` builds out of the standalone Core/GUI/ workspace, so in a
# build tree its binary lands under Core/GUI/target/{release,debug}/ --
# NOT rust/target/ (the gpui workspace is deliberately separate from the
# backend workspace so gpui's heavy graphics deps don't ripple into the
# backend lock file). In an installed stack it sits beside everything
# else in the `current` directory. The resolver knows both layouts; the
# legacy fallback below knows only the build tree.
if (-not $GuiBin) {
    $GuiRoot = Join-Path $WyldeRoot 'Core\GUI'
    $GuiBinCandidates = @(
        (Join-Path $GuiRoot 'target\release\wylde-gui.exe'),
        (Join-Path $GuiRoot 'target\debug\wylde-gui.exe')
    )
    $GuiBin = $GuiBinCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1
    if ($GuiBin) { Log "legacy resolution picked GUI: $GuiBin" }
}

if (-not $GuiBin) {
    Log "ERROR: no wylde-gui.exe found"
    Log "  build with: cargo build --release -p wylde-gui   (from Core/GUI/)"
    Write-Error "Wylde GUI binary missing -- see $LogPath"
    exit 1
}

Log "launching gpui GUI: $GuiBin"
try {
    # Working directory: Core\GUI when it exists, exactly as before -- the
    # GUI may resolve assets relative to it and this launch path is the
    # one that must not change behaviour on a dev rig. Only an installed
    # stack (no Core\GUI in the tree) falls back to the binary's folder.
    $GuiWorkDir = Join-Path $WyldeRoot 'Core\GUI'
    if (-not (Test-Path $GuiWorkDir)) { $GuiWorkDir = Split-Path -Parent $GuiBin }
    Start-Process -FilePath $GuiBin -WorkingDirectory $GuiWorkDir
} catch {
    Log "ERROR: GUI launch failed: $_"
    Write-Error "Failed to launch Wylde GUI -- see $LogPath"
    exit 1
}

Log "launcher: done -- GUI process spawned"
