# install-shortcuts.ps1 -- create the two Wylde desktop shortcuts.
#
#   "Wylde Live" -> launch_wylde.ps1            (the normal release-feel app)
#   "Wylde Dev"  -> tools/dev/wylde-dev.ps1     (hot-reload dev environment)
#
# Both .lnk targets invoke powershell with -File (never -Command -- inline
# command strings trip Defender heuristics). Plain WScript.Shell COM, all
# user-level: no elevation, no UAC. Desktop resolves through the shell
# folder API so OneDrive-redirected desktops work. Re-running overwrites
# the shortcuts in place (idempotent).
#
# Invoke as:  powershell -NoProfile -ExecutionPolicy Bypass -File install-shortcuts.ps1

$ErrorActionPreference = 'Stop'
$RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
$Desktop  = [Environment]::GetFolderPath('Desktop')
$Ps       = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
$shell    = New-Object -ComObject WScript.Shell

function New-WyldeShortcut {
    param(
        [string]$Name,
        [string]$ScriptPath,
        [string]$WorkDir,
        [int]$WindowStyle,   # 1 = normal, 7 = minimized
        [string]$Description
    )
    $lnkPath = Join-Path $Desktop "$Name.lnk"
    $lnk = $shell.CreateShortcut($lnkPath)
    $lnk.TargetPath       = $Ps
    $lnk.Arguments        = "-NoProfile -ExecutionPolicy Bypass -File `"$ScriptPath`""
    $lnk.WorkingDirectory = $WorkDir
    $lnk.WindowStyle      = $WindowStyle
    $lnk.Description      = $Description
    $lnk.Save()
    Write-Host "created: $lnkPath"
}

# Live: the launcher spawns daemon+GUI and exits, so minimize its console.
New-WyldeShortcut `
    -Name 'Wylde Live' `
    -ScriptPath (Join-Path $RepoRoot 'launch_wylde.ps1') `
    -WorkDir $RepoRoot `
    -WindowStyle 7 `
    -Description 'Launch Wylde (release build: lifecycle daemon + GUI)'

# Dev: bacon is a TUI -- it needs a normal, persistent console window.
New-WyldeShortcut `
    -Name 'Wylde Dev' `
    -ScriptPath (Join-Path $RepoRoot 'tools\dev\wylde-dev.ps1') `
    -WorkDir $RepoRoot `
    -WindowStyle 1 `
    -Description 'Wylde dev loop: auto rebuild+relaunch on save, live theme hot-reload'
