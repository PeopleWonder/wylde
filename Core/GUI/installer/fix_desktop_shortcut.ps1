# fix_desktop_shortcut.ps1 -- repoint Desktop\Wylde.lnk at the launcher.
#
# Root cause of "Chat unavailable / no backend services": the desktop
# shortcut pointed straight at wylde-gui.exe, which does NOT start the
# Lifecycle daemon (it assumes the daemon is already up). Launching the
# bare GUI therefore left \\.\pipe\wylde-lifecycle absent, so the harness
# pipe never came up and every required-service panel showed the stub.
#
# The correct entry point is launch_wylde.ps1, which boots the Lifecycle
# daemon FIRST, waits for its pipe, then starts the GUI. This script
# rewrites the shortcut to invoke that launcher (windowless), keeping the
# wylde-gui icon so the desktop tile is unchanged.
#
# Idempotent -- safe to re-run.

$ErrorActionPreference = 'Stop'
$lnk    = Join-Path ([Environment]::GetFolderPath('Desktop')) 'Wylde.lnk'
$ps     = "$env:SystemRoot\System32\WindowsPowerShell\v1.0\powershell.exe"
$root   = (Resolve-Path (Join-Path $PSScriptRoot '..\..\..')).Path
$script = Join-Path $root 'launch_wylde.ps1'
$icon   = Join-Path $root 'Core\GUI\target\release\wylde-gui.exe'

if (-not (Test-Path $script)) { Write-Error "launcher not found: $script"; exit 1 }

$w = New-Object -ComObject WScript.Shell
$s = $w.CreateShortcut($lnk)
$s.TargetPath       = $ps
$s.Arguments        = "-NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File `"$script`""
$s.WorkingDirectory = $root
$s.IconLocation     = "$icon,0"
$s.Description      = 'Launch Wylde (daemon-first via launch_wylde.ps1)'
$s.Save()

# Verify
$v = $w.CreateShortcut($lnk)
Write-Output ("TARGET="  + $v.TargetPath)
Write-Output ("ARGS="    + $v.Arguments)
Write-Output ("WORKDIR=" + $v.WorkingDirectory)
Write-Output ("ICON="    + $v.IconLocation)
