<#
.SYNOPSIS
    Build the per-user Wylde NSIS installer (WyldeSetup-<version>.exe).

.DESCRIPTION
    Three phases:
      1. BUILD   cargo build --release for the gpui GUI (Core/GUI workspace)
                 and the backend service binaries (rust/ workspace).
                 Skipped with -SkipBuild (use when the binaries are already
                 built -- the recommended path on Aaron's box, see the cargo
                 file-lock caveat below).
      2. STAGE   Assemble a clean install tree under release-artifacts/stage:
                   * the committed repo tree via `git archive HEAD`
                     (auto-excludes .venv, data/, logs/, gitignored caches),
                   * the built binaries overlaid where launch_wylde.ps1 looks
                     for them (Core/GUI/target/release/wylde-gui.exe and
                     rust/bin/*.exe).
      3. PACK    Invoke makensis against wylde-installer.nsi to emit
                 release-artifacts/WyldeSetup-<version>.exe.
                 Skipped with -StageOnly (stage, then stop -- useful for
                 inspecting the tree or when NSIS isn't installed yet).

    Per-user, no UAC: see tools/installer/wylde-installer.nsi and
    docs/installer.md.

    CARGO + POWERSHELL CAVEAT: this repo has a history of file-lock flakes
    when cargo runs under PowerShell on the dev box. If a build phase errors
    with a "could not remove / access is denied" on a target file, re-run the
    cargo builds from Git Bash:
        (cd Core/GUI && cargo build --release -p wylde-gui)
        (cd rust && cargo build --release)
    then re-run this script with -SkipBuild.

.PARAMETER Version
    Version string baked into the installer + version.txt. Default
    0.2.0 (matches the workspace version). May be a SemVer
    pre-release (e.g. 0.1.0-alpha.1); the numeric core before any "-suffix"
    is passed to makensis as VI_VERSION for the numeric-only VIProductVersion
    field, while the full string is used everywhere a display version is shown.

.PARAMETER SkipBuild
    Skip the cargo build phase; stage whatever binaries already exist.

.PARAMETER StageOnly
    Stage the tree but do NOT invoke makensis. No NSIS required.

.PARAMETER MakeNsis
    Explicit path to makensis.exe. If omitted, PATH and the standard NSIS
    install dirs are searched.

.EXAMPLE
    # Full build (requires NSIS installed):
    powershell -ExecutionPolicy Bypass -File tools\installer\build-installer.ps1

.EXAMPLE
    # Binaries already built, just (re)stage + pack:
    powershell -ExecutionPolicy Bypass -File tools\installer\build-installer.ps1 -SkipBuild

.EXAMPLE
    # Stage only -- no NSIS needed, inspect release-artifacts\stage:
    powershell -ExecutionPolicy Bypass -File tools\installer\build-installer.ps1 -SkipBuild -StageOnly
#>
[CmdletBinding()]
param(
    [string] $Version = "0.2.0",
    [switch] $SkipBuild,
    [switch] $StageOnly,
    [string] $MakeNsis
)

$ErrorActionPreference = 'Stop'

# -- Paths --------------------------------------------------------------------
$ScriptDir = $PSScriptRoot                              # tools\installer
$RepoRoot  = (Resolve-Path (Join-Path $ScriptDir '..\..')).Path
$NsiScript = Join-Path $ScriptDir 'wylde-installer.nsi'
$ArtifactsDir = Join-Path $RepoRoot 'release-artifacts'
$StageDir     = Join-Path $ArtifactsDir 'stage'
$OutExe       = Join-Path $ArtifactsDir "WyldeSetup-$Version.exe"

# Numeric-only core (everything before the first "-") for the NSIS
# VIProductVersion field, which rejects SemVer pre-release suffixes.
$ViVersion    = ($Version -split '-', 2)[0]

# Backend binaries we never ship: the trainer was cut from the alpha
# (retired scope), and the voice bench is a dev micro-benchmark, not a service.
$BinExclude = @('wylde-trainer.exe', 'wylde-voice-bench.exe')

function Write-Step($msg) { Write-Host "`n==> $msg" -ForegroundColor Cyan }
function Write-Info($msg) { Write-Host "    $msg" -ForegroundColor Gray }

Write-Host "Wylde installer build" -ForegroundColor Green
Write-Info "repo root : $RepoRoot"
Write-Info "version   : $Version"
Write-Info "artifacts : $ArtifactsDir"

# -- Phase 1: BUILD -----------------------------------------------------------
if ($SkipBuild) {
    Write-Step "BUILD skipped (-SkipBuild)"
} else {
    Write-Step "BUILD (cargo build --release)"
    Write-Info "If this trips a file-lock error, see the cargo+PowerShell caveat in this script's help."

    Push-Location (Join-Path $RepoRoot 'Core\GUI')
    try {
        & cargo build --release -p wylde-gui
        if ($LASTEXITCODE -ne 0) { throw "cargo build (wylde-gui) failed with exit $LASTEXITCODE" }
    } finally { Pop-Location }

    Push-Location (Join-Path $RepoRoot 'rust')
    try {
        & cargo build --release
        if ($LASTEXITCODE -ne 0) { throw "cargo build (rust workspace) failed with exit $LASTEXITCODE" }
    } finally { Pop-Location }
}

# Sanity: the GUI binary must exist before we stage.
$GuiExe = Join-Path $RepoRoot 'Core\GUI\target\release\wylde-gui.exe'
if (-not (Test-Path $GuiExe)) {
    throw "wylde-gui.exe not found at $GuiExe. Build it first (drop -SkipBuild, or build from Git Bash)."
}

# -- Phase 2: STAGE -----------------------------------------------------------
Write-Step "STAGE -> $StageDir"

if (Test-Path $ArtifactsDir) { Remove-Item $ArtifactsDir -Recurse -Force }
New-Item -ItemType Directory -Path $StageDir -Force | Out-Null

# 2a. Committed repo tree via git archive (clean, excludes gitignored cruft).
Write-Info "git archive HEAD -> stage"
$ArchiveZip = Join-Path $ArtifactsDir 'tree.zip'
Push-Location $RepoRoot
try {
    & git archive --format=zip -o $ArchiveZip HEAD
    if ($LASTEXITCODE -ne 0) { throw "git archive failed with exit $LASTEXITCODE" }
} finally { Pop-Location }
Expand-Archive -Path $ArchiveZip -DestinationPath $StageDir -Force
Remove-Item $ArchiveZip -Force

# 2b. Overlay the gpui GUI binary where launch_wylde.ps1 expects it.
$GuiStageDir = Join-Path $StageDir 'Core\GUI\target\release'
New-Item -ItemType Directory -Path $GuiStageDir -Force | Out-Null
Copy-Item $GuiExe (Join-Path $GuiStageDir 'wylde-gui.exe') -Force
Write-Info "staged Core/GUI/target/release/wylde-gui.exe"

# 2c. Overlay backend service binaries into rust/bin (launcher's first lookup).
$RustRelease = Join-Path $RepoRoot 'rust\target\release'
$BinStageDir = Join-Path $StageDir 'rust\bin'
New-Item -ItemType Directory -Path $BinStageDir -Force | Out-Null
$staged = @()
Get-ChildItem -Path $RustRelease -Filter 'wylde-*.exe' -File -ErrorAction SilentlyContinue | ForEach-Object {
    if ($BinExclude -contains $_.Name) {
        Write-Info "skipped $($_.Name) (excluded)"
    } else {
        Copy-Item $_.FullName (Join-Path $BinStageDir $_.Name) -Force
        $staged += $_.Name
    }
}
Write-Info "staged rust/bin: $($staged -join ', ')"

# 2d. onnxruntime.dll (voice STT/TTS). Not committed and not always built --
# the models + this DLL are normally fetched on first run (Voice/download_models.py).
# Bundle the DLL only if a build produced one; never the multi-GB ONNX models.
$OrtCandidates = @(
    (Join-Path $RepoRoot 'rust\target\release\onnxruntime.dll'),
    (Join-Path $RepoRoot 'rust\spikes\voice-npu-spike\target\release\onnxruntime.dll')
)
$Ort = $OrtCandidates | Where-Object { Test-Path $_ } | Select-Object -First 1
if ($Ort) {
    Copy-Item $Ort (Join-Path $BinStageDir 'onnxruntime.dll') -Force
    Write-Info "staged rust/bin/onnxruntime.dll  (from $Ort)"
} else {
    Write-Info "onnxruntime.dll not found -- voice models download on first run (see docs/installer.md)"
}

# Stage report.
$stageSize = (Get-ChildItem $StageDir -Recurse -File | Measure-Object -Property Length -Sum).Sum
Write-Info ("stage size: {0:N1} MB" -f ($stageSize / 1MB))

# -- Phase 3: PACK ------------------------------------------------------------
if ($StageOnly) {
    Write-Step "PACK skipped (-StageOnly). Staged tree at: $StageDir"
    return
}

Write-Step "PACK (makensis)"

if (-not $MakeNsis) {
    $cmd = Get-Command makensis -ErrorAction SilentlyContinue
    if ($cmd) { $MakeNsis = $cmd.Source }
}
if (-not $MakeNsis) {
    # Portable NSIS (no-UAC install) -- the recommended path on Aaron's box.
    # Extracted under %USERPROFILE%\Tools\NSIS\nsis-<ver>\; see docs/installer.md.
    # Newest version wins if several are unpacked side by side.
    $MakeNsis = Get-ChildItem "$env:USERPROFILE\Tools\NSIS\*\makensis.exe" -ErrorAction SilentlyContinue |
        Sort-Object FullName -Descending | Select-Object -First 1 -ExpandProperty FullName
}
if (-not $MakeNsis) {
    foreach ($p in @(
        "$env:ProgramFiles\NSIS\makensis.exe",
        "${env:ProgramFiles(x86)}\NSIS\makensis.exe")) {
        if (Test-Path $p) { $MakeNsis = $p; break }
    }
}
if (-not $MakeNsis) {
    Write-Error @"
makensis.exe not found.

Recommended (no UAC): download the portable NSIS zip and extract it under
%USERPROFILE%\Tools\NSIS\  (this script auto-discovers nsis-<ver>\makensis.exe
there). See docs/installer.md "Portable NSIS (no-UAC build host)".

Otherwise install NSIS system-wide (https://nsis.sourceforge.io/Download or
winget install NSIS.NSIS -- both trigger UAC), then add it to PATH or pass
-MakeNsis "C:\Program Files (x86)\NSIS\makensis.exe".

To stage without packing in the meantime, re-run with -StageOnly.
"@
    exit 1
}
Write-Info "makensis: $MakeNsis"

& $MakeNsis `
    "/DVERSION=$Version" `
    "/DVI_VERSION=$ViVersion" `
    "/DSTAGE_DIR=$StageDir" `
    "/DOUT_FILE=$OutExe" `
    $NsiScript
if ($LASTEXITCODE -ne 0) { throw "makensis failed with exit $LASTEXITCODE" }

if (-not (Test-Path $OutExe)) { throw "makensis reported success but $OutExe is missing" }

$outSize = (Get-Item $OutExe).Length
Write-Step "DONE"
Write-Host ("  Installer : {0}" -f $OutExe) -ForegroundColor Green
Write-Host ("  Size      : {0:N1} MB" -f ($outSize / 1MB)) -ForegroundColor Green
Write-Host  "  Install   : per-user, no UAC -> %LOCALAPPDATA%\Programs\Wylde" -ForegroundColor Green
