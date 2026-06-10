# build-watcher.ps1 — self-hosted build/test loop for agent sessions.
#
# Sits in a poll loop watching outputs\build-requests\*.request. Each
# request file lists one TARGET per line (fixed menu below — the watcher
# NEVER executes arbitrary commands from request files). Output + exit
# codes land in outputs\build-results\<id>.result.txt, which the agent
# reads back through the shared folder.
#
# Start via start-build-watcher.bat (or:
#   powershell -NoProfile -ExecutionPolicy Bypass -File tools\build-watcher\build-watcher.ps1
# ). Leave the window open; Ctrl+C stops it. No admin / UAC required.
#
# Targets:
#   backend                 cargo test  (whole rust/ workspace)
#   gui                     cargo test  (whole Core/GUI workspace)
#   test:<crate>            cargo test   -p <crate>   (rust/ workspace)
#   check:<crate>           cargo check  -p <crate>   (rust/ workspace)
#   clippy:<crate>          cargo clippy -p <crate>   (rust/ workspace)
#   gui-test:<crate>        cargo test   -p <crate>   (Core/GUI workspace)
#   gui-check:<crate>       cargo check  -p <crate>   (Core/GUI workspace)
#   gui-clippy:<crate>      cargo clippy -p <crate>   (Core/GUI workspace)
#
# Crate names are validated against ^[a-zA-Z0-9_-]+$ and passed as argv
# elements (no shell interpolation). Unknown targets are skipped and
# noted in the result file.

param([int]$PollSeconds = 2)

$ErrorActionPreference = 'Continue'
$root = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
Set-Location $root

$reqDir  = Join-Path $root 'outputs\build-requests'
$resDir  = Join-Path $root 'outputs\build-results'
$logFile = Join-Path $root 'outputs\build-watcher.log'
$alive   = Join-Path $root 'outputs\build-watcher.alive'
New-Item -ItemType Directory -Force -Path $reqDir, $resDir | Out-Null

function Write-Log([string]$msg) {
    $line = "$(Get-Date -Format o) $msg"
    Add-Content -Path $logFile -Value $line
    Write-Host $line
}

function Invoke-Target([string]$target, [string]$outFile) {
    $t = $target.Trim()
    if (-not $t) { return }
    $cargoArgs = $null
    switch -Regex ($t) {
        '^backend$' {
            $cargoArgs = @('test', '--manifest-path', 'rust/Cargo.toml'); break
        }
        '^gui$' {
            $cargoArgs = @('test', '--manifest-path', 'Core/GUI/Cargo.toml'); break
        }
        '^(test|check|clippy):([a-zA-Z0-9_-]+)$' {
            $cargoArgs = @($Matches[1], '-p', $Matches[2], '--manifest-path', 'rust/Cargo.toml'); break
        }
        '^gui-(test|check|clippy):([a-zA-Z0-9_-]+)$' {
            $cargoArgs = @($Matches[1], '-p', $Matches[2], '--manifest-path', 'Core/GUI/Cargo.toml'); break
        }
        default {
            Add-Content $outFile "=== SKIPPED unknown target: $t ==="
            Write-Log "skipped unknown target: $t"
            return
        }
    }
    Add-Content $outFile "=== cargo $($cargoArgs -join ' ') ==="
    Write-Log "running: cargo $($cargoArgs -join ' ')"
    & cargo @cargoArgs 2>&1 | Out-File -FilePath $outFile -Append -Encoding utf8
    Add-Content $outFile "=== exit: $LASTEXITCODE ==="
    Write-Log "finished (exit=$LASTEXITCODE)"
}

Write-Log "build-watcher up — root=$root poll=${PollSeconds}s"
Write-Log "watching $reqDir"

while ($true) {
    Get-Date -Format o | Set-Content -Path $alive
    $requests = Get-ChildItem -Path $reqDir -Filter '*.request' -ErrorAction SilentlyContinue |
        Sort-Object Name
    foreach ($req in $requests) {
        $id = [IO.Path]::GetFileNameWithoutExtension($req.Name)
        $outFile = Join-Path $resDir "$id.result.txt"
        Write-Log "processing $($req.Name)"
        Set-Content -Path $outFile -Value "=== build-watcher run '$id' @ $(Get-Date -Format o) ==="
        foreach ($line in (Get-Content $req.FullName)) {
            Invoke-Target $line $outFile
        }
        Add-Content $outFile '=== done ==='
        Remove-Item $req.FullName -Force
        Write-Log "done -> $outFile"
    }
    Start-Sleep -Seconds $PollSeconds
}
