# wylde-backend-watch.ps1 -- backend half of the full-stack hot-reload loop.
#
# Companion to the GUI watcher (`bacon dev`). Where bacon rebuilds+relaunches
# the gpui app on a GUI source save, THIS watches the backend service crates
# under rust/crates and, on a save, rebuilds JUST the changed service and
# bounces JUST that service via the Lifecycle daemon's DEV-ONLY
# `dev.restart_service` verb -- the GUI and every other service stay up.
#
# How a bounce stays graceful AND survives a broken build:
#   * The dev daemon spawns each service from a STAGED copy
#     (rust/target-dev/stage/<svc>.exe), set up by wylde-dev.ps1 via the
#     WYLDE_<NAME>_BIN overrides. This watcher builds into
#     rust/target-dev/debug/<svc>.exe -- a DIFFERENT path -- so the compile
#     never fights the running .exe's Windows sharing lock and can run while
#     the old service keeps serving.
#   * On a SUCCESSFUL build it asks the daemon to `dev.restart_service`
#     {name, binary=<fresh debug exe>}: the daemon gracefully stops the
#     service (releasing the stage lock), copies the fresh exe over the stage
#     path, and respawns. On a FAILED build it logs the compiler error and
#     does nothing -- the old service stays up.
#
# Dev-pipeline-scoped, release-untouched (same contract as wylde-dev.ps1):
# its own CARGO_TARGET_DIR=rust/target-dev cache + rust-lld linker, set on
# THIS process only -- never .cargo/config.toml, so release bytes in
# rust/target/ are never invalidated.
#
# Normally launched by wylde-dev.ps1 (which also boots the dev daemon with the
# stage overrides + WYLDE_DEV_HOTRELOAD gate). Can be run standalone for
# backend-only iteration once a dev daemon is up.
#
# Invoke:  powershell -NoProfile -ExecutionPolicy Bypass -File wylde-backend-watch.ps1

param(
    [string]$RepoRoot,
    [int]$DebounceMs = 700,
    [int]$PollMs = 250,
    # Off by default: a shared-lib edit (e.g. wylde-shared) touches many
    # services; auto-bouncing all of them on every save thrashes. With this
    # set, a shared-lib save rebuilds+bounces every dependent service.
    [switch]$FanOutSharedLibs
)

$ErrorActionPreference = 'Continue'  # a watcher must survive transient errors

if (-not $RepoRoot) {
    $RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
}
$RustRoot  = Join-Path $RepoRoot 'rust'
$CratesDir = Join-Path $RustRoot 'crates'

# --- Dev pipeline env (this process only; release bytes untouched) ----
# Forced to the BACKEND dev cache regardless of any inherited value: when
# wylde-dev.ps1 launches us it has already set CARGO_TARGET_DIR to the GUI
# cache for its bacon process, and we must NOT build backend crates there.
$env:CARGO_TARGET_DIR = Join-Path $RustRoot 'target-dev'
if (-not $env:RUSTFLAGS) {
    $env:RUSTFLAGS = '-C linker=rust-lld'
}
$TargetDev = $env:CARGO_TARGET_DIR
$DebugDir  = Join-Path $TargetDev 'debug'
$StageDir  = Join-Path $TargetDev 'stage'
New-Item -ItemType Directory -Force -Path $StageDir | Out-Null

# --- crate -> service map ---------------------------------------------
# DAEMON-MANAGED services (1:1 crate==service==bin name): rebuilt and bounced
# in place via the daemon's dev.restart_service verb. wylde-memgraph is
# excluded -- it supervises the bundled Neo4j JVM, not a rebuildable Rust
# binary. This set mirrors DAEMON_MANAGED_SERVICES in
# rust/crates/wylde-lifecycle/src/control.rs (minus memgraph).
$DaemonServices = @(
    'wylde-workspaces',
    'wylde-harness',
    'wylde-gateway',
    'wylde-ollama',
    'wylde-vram-broker',
    'wylde-device-gate',
    'wylde-extension-bridge',
    'wylde-voice',
    'wylde-vpn',
    'wylde-treesitter',
    'wylde-n8n'
)

# STAGE-ONLY services: real service binaries that the daemon does NOT
# supervise (wylde-lsp is GUI-spawned, lazily, on the first lsp.open). We
# rebuild + drop the fresh exe into the stage path so the next time it is
# launched it runs new bytes -- but we can't ask the daemon to bounce it.
$StageOnlyServices = @(
    'wylde-lsp'
)

# SHARED libs -> the services that must rebuild+bounce when they change.
# Only consulted when -FanOutSharedLibs is set (see the param note).
$SharedLibFanout = @{
    'wylde-shared'             = $DaemonServices + $StageOnlyServices
    'wylde-workspaces-client'  = @('wylde-workspaces')
    'wylde-treesitter'         = @('wylde-treesitter')  # also a service crate; harmless overlap
}

# Crates we actively poll == all of the above keys (union).
$WatchCrates = @($DaemonServices + $StageOnlyServices + $SharedLibFanout.Keys) |
    Sort-Object -Unique |
    Where-Object { Test-Path (Join-Path $CratesDir $_) }

Write-Host "Wylde backend watch -- repo: $RepoRoot"
Write-Host "target dir : $TargetDev"
Write-Host "linker     : rust-lld (dev only)"
Write-Host "watching   : $($WatchCrates.Count) crate(s) under rust/crates"
Write-Host "debounce   : ${DebounceMs}ms  fan-out-shared-libs: $($FanOutSharedLibs.IsPresent)"

# --- one-time: build the ipc_call dev client ---------------------------
# Lives in wylde-shared/examples so it links while the stack is up (lib
# crate -> no prebuild guard). We build it once and invoke the exe per
# bounce (fast; no cargo overhead on the hot path).
$IpcCall = Join-Path $DebugDir 'examples\ipc_call.exe'
Write-Host "building dev ipc client (ipc_call)..."
Push-Location $RustRoot
& cargo build -q -p wylde-shared --example ipc_call 2>&1 | Out-Host
Pop-Location
if (-not (Test-Path $IpcCall)) {
    Write-Warning "ipc_call.exe not found at $IpcCall -- bounces will be skipped (build only)."
}

# --- helpers -----------------------------------------------------------
# Write a UTF-8 file with NO byte-order mark. `Set-Content -Encoding utf8` in
# Windows PowerShell 5.1 prepends a BOM (EF BB BF); ipc_call reads the JSON
# payload byte-for-byte and rejects a leading BOM as invalid JSON, so every
# bounce would silently fail. This guarantees BOM-free bytes.
function Write-Utf8NoBom([string]$path, [string]$text) {
    [System.IO.File]::WriteAllText($path, $text, (New-Object System.Text.UTF8Encoding($false)))
}

function Get-CrateMaxMtime([string]$crate) {
    $dir = Join-Path $CratesDir $crate
    $files = Get-ChildItem -Path $dir -Recurse -File -Include *.rs, *.toml -ErrorAction SilentlyContinue
    if (-not $files) { return [datetime]::MinValue }
    ($files | Measure-Object -Property LastWriteTimeUtc -Maximum).Maximum
}

# Expand a changed crate into the list of services to refresh.
function Resolve-Targets([string]$crate) {
    $targets = New-Object System.Collections.Generic.List[string]
    if ($DaemonServices -contains $crate)    { $targets.Add($crate) }
    if ($StageOnlyServices -contains $crate) { $targets.Add($crate) }
    if ($FanOutSharedLibs -and $SharedLibFanout.ContainsKey($crate)) {
        foreach ($s in $SharedLibFanout[$crate]) { if (-not $targets.Contains($s)) { $targets.Add($s) } }
    }
    elseif ($SharedLibFanout.ContainsKey($crate) -and -not ($DaemonServices -contains $crate) -and -not ($StageOnlyServices -contains $crate)) {
        Write-Host "  [note] $crate is a shared lib; affects $($SharedLibFanout[$crate].Count) service(s). Re-run with -FanOutSharedLibs to auto-bounce them." -ForegroundColor DarkYellow
    }
    return $targets
}

function Invoke-Bounce([string]$service) {
    $stamp = (Get-Date).ToString('HH:mm:ss')
    Write-Host "[$stamp] rebuilding $service ..." -ForegroundColor Cyan
    $t0 = Get-Date
    Push-Location $RustRoot
    $buildOut = & cargo build -p $service --bin $service 2>&1
    $code = $LASTEXITCODE
    Pop-Location
    $secs = [math]::Round(((Get-Date) - $t0).TotalSeconds, 1)
    if ($code -ne 0) {
        Write-Host "[$stamp] BUILD FAILED ($service, ${secs}s) -- $service left running:" -ForegroundColor Red
        ($buildOut | Select-Object -Last 25) | ForEach-Object { Write-Host "    $_" -ForegroundColor DarkRed }
        return
    }
    $exe = Join-Path $DebugDir "$service.exe"
    if (-not (Test-Path $exe)) {
        Write-Host "[$stamp] build ok but exe missing: $exe -- skipping bounce" -ForegroundColor Red
        return
    }

    if ($StageOnlyServices -contains $service) {
        # Not daemon-managed: stage the fresh bytes; next launch picks them up.
        $stage = Join-Path $StageDir "$service.exe"
        Copy-Item -Path $exe -Destination $stage -Force
        Write-Host "[$stamp] $service rebuilt (${secs}s) + staged ($stage). GUI-spawned/optional -- restart it to load new bytes." -ForegroundColor Yellow
        return
    }

    # Daemon-managed: ask the dev daemon to graceful-bounce + binary-swap.
    if (-not (Test-Path $IpcCall)) {
        Write-Host "[$stamp] $service rebuilt (${secs}s) but ipc_call missing -- cannot bounce." -ForegroundColor Red
        return
    }
    $payloadFile = Join-Path $env:TEMP "wylde-hotreload-$service.json"
    $payloadJson = @{ name = $service; binary = $exe } | ConvertTo-Json -Compress
    Write-Utf8NoBom $payloadFile $payloadJson
    $reply = & $IpcCall lifecycle dev.restart_service "@$payloadFile" 2>&1
    $rc = $LASTEXITCODE
    Remove-Item $payloadFile -ErrorAction SilentlyContinue
    if ($rc -eq 0) {
        Write-Host "[$stamp] $service rebuilt (${secs}s) + bounced OK." -ForegroundColor Green
    }
    else {
        Write-Host "[$stamp] $service rebuilt (${secs}s) but bounce returned an error:" -ForegroundColor Red
        ($reply | Out-String).Trim() -split "`n" | ForEach-Object { Write-Host "    $_" -ForegroundColor DarkRed }
    }
}

# --- the poll loop -----------------------------------------------------
$state = @{}
foreach ($c in $WatchCrates) {
    $state[$c] = @{ MaxMtime = (Get-CrateMaxMtime $c); Pending = $false; LastChange = $null }
}
Write-Host ""
Write-Host "watching for backend source saves (Ctrl-C to stop) ..." -ForegroundColor Green

while ($true) {
    $now = Get-Date
    foreach ($c in $WatchCrates) {
        $m = Get-CrateMaxMtime $c
        $st = $state[$c]
        if ($m -gt $st.MaxMtime) {
            $st.MaxMtime = $m
            $st.Pending = $true
            $st.LastChange = $now
        }
        elseif ($st.Pending -and (($now - $st.LastChange).TotalMilliseconds -ge $DebounceMs)) {
            $st.Pending = $false
            foreach ($svc in (Resolve-Targets $c)) { Invoke-Bounce $svc }
        }
    }
    Start-Sleep -Milliseconds $PollMs
}
