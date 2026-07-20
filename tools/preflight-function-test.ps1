<#
.SYNOPSIS
    Read-only pre-flight for a hands-on Wylde function test. Surfaces anything
    that would break the run before you launch the GUI.

.DESCRIPTION
    Checks (all read-only, no admin, no UAC):
      1. Service binaries present (wylde-gui + the rust/target/release set).
      2. Service manifests present + required launcher keys populated.
      3. Named-pipe collisions (\\.\pipe\wylde-*  already in use).
      4. Declared HTTP ports free (from data/manifests + rust/data/manifests).
      5. Ollama reachable (http://127.0.0.1:11434/api/tags, 2s) - blocking,
         the harness manifest names ollama as its model backend.
      6. AdGuard service status (Event 7034 has knocked it over before).
      7. WireGuard tunnel adapter (wg0 / wg1 / Wylde) - opt-in, informational.
      8. wylde_check run_all == 0/0/0 across 30 rules.

    Prints a green/yellow/red status table at the end. ASCII-only so it parses
    under Windows PowerShell 5.1 regardless of file encoding.

.PARAMETER RepoRoot
    Wylde repo root. Defaults to the parent of this script's folder, or the
    current directory if pasted inline.

.PARAMETER SkipWyldeCheck
    Skip the wylde_check run (it shells out to `uv run`, the slowest check).

.EXAMPLE
    pwsh -NoProfile -File tools\preflight-function-test.ps1
#>
[CmdletBinding()]
param(
    [string] $RepoRoot,
    [switch] $SkipWyldeCheck
)

$ErrorActionPreference = 'Continue'

# -- Resolve repo root --------------------------------------------------------
if (-not $RepoRoot) {
    if ($PSScriptRoot) { $RepoRoot = Split-Path -Parent $PSScriptRoot }
    else               { $RepoRoot = (Get-Location).Path }
}
$RepoRoot = (Resolve-Path -LiteralPath $RepoRoot).Path

# -- Result collector ---------------------------------------------------------
$results = New-Object System.Collections.ArrayList
function Add-Result {
    param(
        [string] $Section,
        [string] $Check,
        [ValidateSet('GREEN', 'YELLOW', 'RED')] [string] $Status,
        [string] $Detail
    )
    [void]$results.Add([pscustomobject]@{
        Section = $Section; Check = $Check; Status = $Status; Detail = $Detail
    })
    $color = switch ($Status) { 'GREEN' { 'Green' } 'YELLOW' { 'Yellow' } 'RED' { 'Red' } }
    Write-Host ("  [{0,-6}] {1,-34} {2}" -f $Status, $Check, $Detail) -ForegroundColor $color
}

Write-Host "Wylde pre-flight  (repo: $RepoRoot)" -ForegroundColor Cyan
Write-Host ""

# -- 1. Binaries --------------------------------------------------------------
Write-Host "== Binaries ==" -ForegroundColor Cyan
# wylde-gui lives in the standalone Core/GUI/ workspace, not rust/target.
$guiCandidates = @(
    (Join-Path $RepoRoot 'Core\GUI\target\release\wylde-gui.exe'),
    (Join-Path $RepoRoot 'rust\target\release\wylde-gui.exe')
)
$guiPath = $guiCandidates | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
if ($guiPath) { Add-Result 'bin' 'wylde-gui.exe' 'GREEN' $guiPath }
else          { Add-Result 'bin' 'wylde-gui.exe' 'RED'  'missing (build Core/GUI/ workspace)' }

$rel = Join-Path $RepoRoot 'rust\target\release'
# name -> severity when missing
$binSpec = @(
    @{ n = 'wylde-harness.exe';     sev = 'RED'    },
    @{ n = 'wylde-gateway.exe';     sev = 'RED'    },
    @{ n = 'wylde-device-gate.exe'; sev = 'RED'    },
    @{ n = 'wylde-lifecycle.exe';   sev = 'RED'    },
    @{ n = 'wylde-voice.exe';       sev = 'RED'    },
    @{ n = 'wylde-vpn.exe';         sev = 'YELLOW' }   # opt-in / default-off
)
foreach ($b in $binSpec) {
    $p = Join-Path $rel $b.n
    if (Test-Path -LiteralPath $p) { Add-Result 'bin' $b.n 'GREEN' $p }
    else                           { Add-Result 'bin' $b.n $b.sev 'missing from rust/target/release' }
}
# memgraph has no rust binary - it is python:Core.Memgraph.run supervised by lifecycle.
$mg = Join-Path $rel 'wylde-memgraph.exe'
if (Test-Path -LiteralPath $mg) { Add-Result 'bin' 'wylde-memgraph.exe' 'GREEN' $mg }
else { Add-Result 'bin' 'wylde-memgraph.exe' 'GREEN' 'n/a - Python (Core.Memgraph.run), not a rust binary' }

# -- 2. Manifests -------------------------------------------------------------
Write-Host ""
Write-Host "== Service manifests ==" -ForegroundColor Cyan
# The launcher (Core/Lifecycle/manifest.py) schema keys:
#   entry_point (== start command; null = library/pipe-only/in-process)
#   depends_on  (== required services)
#   tier        (standard|core|optional|extension)
# Ports are NOT in these manifests; they live in data/manifests/*.json (checked
# in the ports section). We discover service folders the launcher would scan:
# top-level <root>\<svc>\manifest.json and <root>\Core\<svc>\manifest.json.
# Container folders that are themselves scan *bases* (we descend into their
# children below) rather than leaf services. Their own top-level manifest.json
# is package-root/aggregate metadata - e.g. Core\manifest.json describes the
# bundle via `constituent_pipes` and has no `entry_point`/`depends_on` because
# Core is a namespace, not a launchable process (the real services live at
# Core\<ServiceName>\manifest.json with the full launcher schema). Skip these
# so the scan only flags genuine leaf-service manifests.
$containerDirs = @((Join-Path $RepoRoot 'Core'))
$svcManifests = @()
foreach ($base in @($RepoRoot, (Join-Path $RepoRoot 'Core'))) {
    Get-ChildItem -LiteralPath $base -Directory -ErrorAction SilentlyContinue | ForEach-Object {
        if ($containerDirs -contains $_.FullName) { return }  # aggregate, not a leaf service
        $mf = Join-Path $_.FullName 'manifest.json'
        if (Test-Path -LiteralPath $mf) { $svcManifests += $mf }
    }
}
$requiredKeys = @('name', 'entry_point', 'depends_on', 'tier')
foreach ($mf in ($svcManifests | Sort-Object)) {
    $rel2 = $mf.Replace($RepoRoot + [IO.Path]::DirectorySeparatorChar, '')
    try {
        $j = Get-Content -LiteralPath $mf -Raw -ErrorAction Stop | ConvertFrom-Json -ErrorAction Stop
    }
    catch {
        Add-Result 'manifest' $rel2 'RED' "malformed JSON: $($_.Exception.Message)"
        continue
    }
    $missing = @()
    foreach ($k in $requiredKeys) {
        if (-not ($j.PSObject.Properties.Name -contains $k)) { $missing += $k }
    }
    if ($missing.Count -gt 0) {
        Add-Result 'manifest' $rel2 'RED' ("missing keys: " + ($missing -join ', '))
    }
    else {
        $ep = if ($null -eq $j.entry_point -or $j.entry_point -eq '') { 'library/pipe-only' } else { $j.entry_point }
        Add-Result 'manifest' $rel2 'GREEN' ("tier=$($j.tier) entry=$ep")
    }
}
if ($svcManifests.Count -eq 0) {
    Add-Result 'manifest' '(discovery)' 'RED' 'no service manifests found - wrong RepoRoot?'
}

# -- 3. Named-pipe collisions -------------------------------------------------
Write-Host ""
Write-Host "== Named pipes (\\.\pipe\wylde-*) ==" -ForegroundColor Cyan
try {
    $allPipes = [System.IO.Directory]::GetFiles('\\.\pipe\')
    $wyldePipes = $allPipes | Where-Object { $_ -match 'wylde-' }
    if ($wyldePipes.Count -eq 0) {
        Add-Result 'pipe' 'wylde-* pipes' 'GREEN' 'none in use - clean slate'
    }
    else {
        foreach ($p in $wyldePipes) {
            $name = Split-Path -Leaf $p
            Add-Result 'pipe' $name 'YELLOW' 'in use - a prior Wylde instance may still be running'
        }
    }
}
catch {
    Add-Result 'pipe' 'enumerate pipes' 'YELLOW' "could not list pipes: $($_.Exception.Message)"
}

# -- 4. Declared port collisions ----------------------------------------------
Write-Host ""
Write-Host "== Declared HTTP ports ==" -ForegroundColor Cyan
function Test-PortListening {
    param([int] $Port)
    $client = New-Object System.Net.Sockets.TcpClient
    try {
        $iar = $client.BeginConnect('127.0.0.1', $Port, $null, $null)
        $ok = $iar.AsyncWaitHandle.WaitOne(400)
        if ($ok -and $client.Connected) { return $true }
        return $false
    }
    catch { return $false }
    finally { $client.Close() }
}
$portMap = @{}   # port -> service (dedupe across data/manifests + rust/data/manifests)
foreach ($mdir in @((Join-Path $RepoRoot 'data\manifests'), (Join-Path $RepoRoot 'rust\data\manifests'))) {
    if (-not (Test-Path -LiteralPath $mdir)) { continue }
    Get-ChildItem -LiteralPath $mdir -Filter '*.json' -File | ForEach-Object {
        try {
            $j = Get-Content -LiteralPath $_.FullName -Raw | ConvertFrom-Json
            if ($j.port -and [int]$j.port -gt 0) {
                $portMap[[int]$j.port] = $j.service
            }
        }
        catch { }
    }
}
if ($portMap.Count -eq 0) {
    Add-Result 'port' '(discovery)' 'YELLOW' 'no positive ports declared in data manifests'
}
foreach ($port in ($portMap.Keys | Sort-Object)) {
    $svc = $portMap[$port]
    if (Test-PortListening -Port $port) {
        Add-Result 'port' ("$port ($svc)") 'YELLOW' 'in use - service already up or port conflict'
    }
    else {
        Add-Result 'port' ("$port ($svc)") 'GREEN' 'free'
    }
}

# -- 5. Ollama ----------------------------------------------------------------
Write-Host ""
Write-Host "== Ollama (model backend) ==" -ForegroundColor Cyan
try {
    $resp = Invoke-WebRequest -Uri 'http://127.0.0.1:11434/api/tags' -TimeoutSec 2 -UseBasicParsing -ErrorAction Stop
    if ($resp.StatusCode -eq 200) {
        $tagCount = 0
        try { $tagCount = (($resp.Content | ConvertFrom-Json).models).Count } catch { }
        Add-Result 'ollama' '127.0.0.1:11434' 'GREEN' "reachable ($tagCount model(s) installed)"
    }
    else {
        Add-Result 'ollama' '127.0.0.1:11434' 'RED' "HTTP $($resp.StatusCode)"
    }
}
catch {
    Add-Result 'ollama' '127.0.0.1:11434' 'RED' 'unreachable - chat will fail (harness needs ollama)'
}

# -- 6. AdGuard ---------------------------------------------------------------
Write-Host ""
Write-Host "== AdGuard service ==" -ForegroundColor Cyan
$adg = Get-Service -Name '*AdGuard*' -ErrorAction SilentlyContinue
if (-not $adg) {
    Add-Result 'adguard' 'AdGuard service' 'YELLOW' 'not installed/registered (ok unless you rely on it)'
}
else {
    foreach ($s in $adg) {
        if ($s.Status -eq 'Running') {
            Add-Result 'adguard' $s.Name 'GREEN' 'Running'
        }
        else {
            Add-Result 'adguard' $s.Name 'YELLOW' "$($s.Status) - Event 7034 has stopped this before; restart if DNS misbehaves"
        }
    }
}

# -- 7. WireGuard tunnel ------------------------------------------------------
Write-Host ""
Write-Host "== WireGuard tunnel ==" -ForegroundColor Cyan
$wgAdapters = Get-NetAdapter -ErrorAction SilentlyContinue |
    Where-Object { $_.Name -match '^(wg0|wg1|Wylde)$' -or $_.InterfaceDescription -match 'WireGuard|wintun' }
if (-not $wgAdapters) {
    Add-Result 'wireguard' 'wg0/wg1/Wylde adapter' 'YELLOW' 'no tunnel adapter (VPN is opt-in - fine if testing locally)'
}
else {
    foreach ($a in $wgAdapters) {
        $st = $a.Status
        $sev = if ($st -eq 'Up') { 'GREEN' } else { 'YELLOW' }
        Add-Result 'wireguard' $a.Name $sev "$st ($($a.InterfaceDescription))"
    }
}

# -- 8. wylde_check -----------------------------------------------------------
Write-Host ""
Write-Host "== wylde_check (30 rules) ==" -ForegroundColor Cyan
if ($SkipWyldeCheck) {
    Add-Result 'wylde_check' 'run_all' 'YELLOW' 'skipped (-SkipWyldeCheck)'
}
else {
    # PS 5.1 mangles the quoting of an inline `-c` snippet that contains both
    # double quotes and `{}` format braces (works under PS7, not 5.1). Write the
    # snippet to a temp .py file and run that instead - identical under 5.1 / 7.
    # Randomized name so concurrent preflight runs don't collide on the temp file.
    $py = @'
import sys
sys.path.insert(0, "")  # CWD (== RepoRoot via Push-Location); `python file.py` omits it, unlike `-c`
from Core.harness.dev.wylde_check import run_all
r = run_all()
s = r["data"]["summary"]["by_severity"]
print("WC {}/{}/{} rules={}".format(s["error"], s["warning"], s["info"], r["data"]["rules_checked"]))
'@
    $tmpPy = Join-Path $env:TEMP ("wylde_check_preflight_{0}.py" -f [guid]::NewGuid().ToString('N'))
    Set-Content -LiteralPath $tmpPy -Value $py -Encoding ASCII
    Push-Location $RepoRoot
    try {
        $out = & uv run python $tmpPy 2>&1 | Out-String
        $code = $LASTEXITCODE
        $line = ($out -split "`n" | Where-Object { $_ -match '^WC ' } | Select-Object -Last 1)
        if ($line) { $line = $line.Trim() }
        if ($line -match '^WC (\d+)/(\d+)/(\d+) rules=(\d+)') {
            $err = [int]$Matches[1]; $warn = [int]$Matches[2]; $info = [int]$Matches[3]; $rules = [int]$Matches[4]
            $sev = if ($err -eq 0 -and $warn -eq 0 -and $info -eq 0) { 'GREEN' } elseif ($err -gt 0) { 'RED' } else { 'YELLOW' }
            Add-Result 'wylde_check' 'run_all' $sev "$err/$warn/$info across $rules rules"
        }
        elseif ($code -ne 0) {
            # uv run failed and produced no parseable WC line - surface as RED, not silent.
            Add-Result 'wylde_check' 'run_all' 'RED' ("uv run exited ${code}: " + ($out.Trim() -replace "`r?`n", ' | '))
        }
        else {
            Add-Result 'wylde_check' 'run_all' 'RED' ("could not parse output: " + ($out.Trim() -replace "`r?`n", ' | '))
        }
    }
    catch {
        Add-Result 'wylde_check' 'run_all' 'RED' "invocation failed: $($_.Exception.Message)"
    }
    finally {
        Pop-Location
        Remove-Item -LiteralPath $tmpPy -Force -ErrorAction SilentlyContinue
    }
}

# -- Summary table ------------------------------------------------------------
$red    = @($results | Where-Object { $_.Status -eq 'RED' })
$yellow = @($results | Where-Object { $_.Status -eq 'YELLOW' })
$green  = @($results | Where-Object { $_.Status -eq 'GREEN' })

Write-Host ""
Write-Host "================ PRE-FLIGHT SUMMARY ================" -ForegroundColor Cyan
Write-Host ("  GREEN : {0}" -f $green.Count)  -ForegroundColor Green
Write-Host ("  YELLOW: {0}" -f $yellow.Count) -ForegroundColor Yellow
Write-Host ("  RED   : {0}" -f $red.Count)    -ForegroundColor Red
Write-Host "===================================================" -ForegroundColor Cyan

if ($red.Count -gt 0) {
    Write-Host "`nBLOCKING (red) - fix before launching:" -ForegroundColor Red
    $red | ForEach-Object { Write-Host ("  - {0}: {1}" -f $_.Check, $_.Detail) -ForegroundColor Red }
}
if ($yellow.Count -gt 0) {
    Write-Host "`nWARNINGS (yellow) - review, may be fine:" -ForegroundColor Yellow
    $yellow | ForEach-Object { Write-Host ("  - {0}: {1}" -f $_.Check, $_.Detail) -ForegroundColor Yellow }
}
if ($red.Count -eq 0) {
    Write-Host "`nNo blockers. Clear to launch the GUI." -ForegroundColor Green
}

# Non-zero exit if anything is blocking, so this can gate a launch script.
exit ([int]($red.Count -gt 0))
