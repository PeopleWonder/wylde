# relaunch-backend-only.ps1 — bring up ONLY the Wylde lifecycle daemon
# (which spawns every service incl. the discovered wylde-tabulate sibling).
#
# Unlike launch_wylde.ps1 this does NOT launch the GUI — for the tabulate
# feel-test the backend is brought up here and the operator opens the GUI
# themselves. Run from the worktree so WYLDE_ROOT resolves to it and dynamic
# discovery picks up Services/wylde-tabulate (enabled).

$ErrorActionPreference = 'Stop'
$WyldeRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$Daemon = Join-Path $WyldeRoot 'rust\target\release\wylde-lifecycle.exe'
if (-not (Test-Path $Daemon)) { Write-Error "daemon missing: $Daemon"; exit 1 }

Write-Host "WYLDE_ROOT = $WyldeRoot"
Write-Host "daemon     = $Daemon"
$proc = Start-Process -FilePath $Daemon -WorkingDirectory $WyldeRoot -WindowStyle Hidden -PassThru
Write-Host "daemon spawned: pid=$($proc.Id)"

# Wait for the lifecycle pipe to bind.
$deadline = (Get-Date).AddSeconds(30)
$ready = $false
while ((Get-Date) -lt $deadline) {
    try {
        $pipes = Get-ChildItem '\\.\pipe\' -ErrorAction Stop | Select-Object -ExpandProperty Name
        if ($pipes -contains 'wylde-lifecycle') { $ready = $true; break }
    } catch {}
    Start-Sleep -Milliseconds 250
}
if ($ready) { Write-Host 'lifecycle pipe up — backend ready' }
else { Write-Error 'lifecycle pipe did not come up within 30s'; exit 1 }
