# wylde-dev.ps1 -- the "Wylde Dev" hot-iteration environment.
#
# What it does, in order:
#   1. Boots the Rust Lifecycle daemon (same binary + pipe-wait as
#      launch_wylde.ps1) IF the lifecycle pipe isn't already up, so the
#      dev GUI talks to real services. Fail-soft: if the daemon binary is
#      missing or the pipe never appears, the GUI still launches and every
#      panel degrades gracefully (OI-1 banners).
#   2. Sets the dev-only acceleration + hot-reload environment:
#        CARGO_TARGET_DIR = Core/GUI/target-dev
#            A SEPARATE incremental cache for the dev loop, so dev builds
#            and the build-watcher/release builds (plain target/) never
#            invalidate each other's fingerprints.
#        RUSTFLAGS = -C linker=rust-lld
#            rust-lld ships with the rustup toolchain (no install) and
#            links the dev binary several times faster than MSVC link.exe.
#            Scoped HERE -- not .cargo/config.toml -- so the shipped
#            release build's linker is untouched (cargo cannot scope a
#            linker per-profile on stable; see the dev-env report).
#        WYLDE_THEME_PATH = <repo>/Core/GUI/Frontend/Panels/Workspaces/
#                           assets/visual_style_v1.yaml
#            Debug builds read the Visual Style YAML from THIS file at
#            runtime and hot-reload it on save (graph repaints live;
#            composer styling applies next repaint). Release builds ignore
#            the variable entirely (the embedded asset ships unchanged).
#   3. Hands off to `bacon dev` (Core/GUI/bacon.toml): save a .rs file ->
#      kill the running app -> incremental rebuild -> relaunch.
#
# Invoke as:  powershell -NoProfile -ExecutionPolicy Bypass -File wylde-dev.ps1
# (the "Wylde Dev" desktop shortcut does exactly that). No elevation.
#
# First run builds the whole GUI workspace into target-dev (one-time,
# several minutes); every save after that is an incremental rebuild.

$ErrorActionPreference = 'Stop'
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$GuiRoot  = Join-Path $RepoRoot 'Core\GUI'

Write-Host "Wylde Dev -- repo: $RepoRoot"

# --- 1. Lifecycle daemon (skip if the pipe is already up) -----------
$pipeUp = $false
try {
    $pipes = Get-ChildItem '\\.\pipe\' -ErrorAction Stop | Select-Object -ExpandProperty Name
    $pipeUp = $pipes -contains 'wylde-lifecycle'
} catch {}

if (-not $pipeUp) {
    $daemonCandidates = @(
        (Join-Path $RepoRoot 'rust\bin\wylde-lifecycle.exe'),
        (Join-Path $RepoRoot 'rust\target\release\wylde-lifecycle.exe'),
        (Join-Path $RepoRoot 'rust\target\debug\wylde-lifecycle.exe')
    )
    $daemon = $daemonCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1
    if ($daemon) {
        Write-Host "starting lifecycle daemon: $daemon"
        Start-Process -FilePath $daemon -WorkingDirectory $RepoRoot -WindowStyle Hidden | Out-Null
        $deadline = (Get-Date).AddSeconds(20)
        while ((Get-Date) -lt $deadline) {
            try {
                $pipes = Get-ChildItem '\\.\pipe\' -ErrorAction Stop |
                         Select-Object -ExpandProperty Name
                if ($pipes -contains 'wylde-lifecycle') { $pipeUp = $true; break }
            } catch {}
            Start-Sleep -Milliseconds 250
        }
    }
    if (-not $pipeUp) {
        Write-Warning "lifecycle pipe not up -- the dev GUI will run with degraded services (OI-1 banners)."
    }
} else {
    Write-Host "lifecycle pipe already up -- reusing the running stack"
}

# --- 2. Dev-only environment ---------------------------------------
$env:CARGO_TARGET_DIR = Join-Path $GuiRoot 'target-dev'
$env:RUSTFLAGS        = '-C linker=rust-lld'
$env:WYLDE_THEME_PATH = Join-Path $GuiRoot 'Frontend\Panels\Workspaces\assets\visual_style_v1.yaml'

Write-Host "target dir : $env:CARGO_TARGET_DIR"
Write-Host "linker     : rust-lld (dev only)"
Write-Host "theme hot  : $env:WYLDE_THEME_PATH"
Write-Host ""
Write-Host "bacon dev -- save a .rs file to rebuild+relaunch; edit the YAML to restyle live; q quits."

# --- 3. The watch loop ----------------------------------------------
Set-Location $GuiRoot
bacon dev
