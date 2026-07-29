<#
.SYNOPSIS
    Self-running, automated CLEAN-INSTALL preflight (issue #37): fresh checkout ->
    build -> launch -> assert the stack comes up clean -> teardown. Entirely
    automation-side; NO human reimage.

.DESCRIPTION
    The 0.2 gate-2 clean-install requirement used to mean a human reimages a
    virgin profile/VM and watches the stack come up. That is un-repeatable and
    the maintainer declined it. This script keeps the one property a warm rig
    cannot fake -- a genuinely EMPTY starting profile -- without the human
    reimage, by
    orchestrating the launch-and-verify gate that already exists
    (`wylde-release preflight --launch`):

      1. THROWAWAY FRESH CHECKOUT. Clone the source repo at a chosen ref into a
         scratch directory this run owns and deletes afterward -- not the working
         tree, not a warm rig in place. Proves the build from a clean source.
      2. ISOLATED, EMPTY DATA ROOT. Point WYLDE_DATA_DIR at a fresh temp dir so
         the stack cold-starts against an empty profile: no populated Wylde data,
         no stale settings. (WYLDE_DATA_DIR wins over the whole DATA_DIR /
         WYLDE_ROOT resolution chain -- see rust/crates/wylde-shared/src/paths.rs.)
      3. BUILD + LAUNCH + ASSERT. Run `wylde-release preflight --launch --build`
         from the fresh checkout, from a neutral working directory: it builds the
         backend + GUI release artifacts, cold-starts the daemon and services,
         and folds each L2/L3/L5 check into a commit-bound receipt (daemon up,
         services healthy, Memgraph has data, RAG answers, a chat turn completes,
         reasoning disabled). Each check fails closed.
      4. TEARDOWN. `wylde-release` tears the stack down itself (graceful
         service.shutdown_all + taskkill backstop, and it never kills an already-
         attached daemon). This script then removes the throwaway checkout and
         data root, so nothing is left on the rig.

    The point: turn "clean install verified" from *a human reimages a machine and
    watches* into *a script anyone can re-run on the release rig and get a
    pass/fail receipt from*.

    WHAT THIS IS NOT. It is not a new gate -- it is orchestration around the
    existing one. And it is not a substitute for the L6 human feel test (a
    separate, required gate-3 ship blocker, issue #274): this proves the stack
    comes up clean mechanically, not that it *feels* right.

.PARAMETER Ref
    Git ref to check out into the throwaway tree (a branch, tag, or SHA). Default
    'origin/develop' -- the promotion candidate. Resolved to a SHA in the source
    repo before the fresh checkout, so the run is pinned to one commit.

.PARAMETER RepoRoot
    Source repo to clone from. Defaults to the parent of this script's folder
    (i.e. the repo this script lives in). The clone is taken from the repo's
    common git dir, so this works whether RepoRoot is a normal checkout or a
    linked worktree.

.PARAMETER WorkDir
    Parent directory for this run's throwaway tree + data root. Defaults to
    "$env:TEMP\wylde-self-preflight". A per-run subfolder is created under it.

.PARAMETER SeedDataFrom
    Optional path to a reference Wylde data dir to copy into the clean profile
    BEFORE launch. A truly EMPTY profile cannot pass the data-dependent L3 legs
    (l3.memgraph_has_data needs an indexed graph; l3.ollama_model needs models
    pulled), so a fully launch-verified run needs the host services in a serving
    state. Point this at a known-good, already-indexed profile to make
    launch_verified achievable while the *code* is still built clean. Omit it to
    run a strict-empty cold-start (daemon/services/chat legs still exercised; the
    data-dependent legs will fail closed -- that is honest, not a bug).

.PARAMETER HostLabel
    Recorded in the receipt's host.label so it is clear which host produced it.
    Default 'automated-clean-install-preflight'. (Deliberately a role string, not
    a person's name -- the wylde-release default host label is a personal name
    and must not land here.)

.PARAMETER KeepWorkDir
    Do not delete the throwaway checkout + data root on exit. For debugging a
    failed run.

.PARAMETER SkipTeardownCheck
    Reserved: teardown of the launched stack is owned by wylde-release itself;
    this switch is a no-op kept for forward-compat and discoverability.

.EXAMPLE
    # Strict-empty cold-start of origin/develop (data-dependent L3 legs will FAIL):
    pwsh -NoProfile -File tools\self-preflight.ps1

.EXAMPLE
    # Fully launch-verifiable run: clean code build, profile seeded to a serving baseline:
    pwsh -NoProfile -File tools\self-preflight.ps1 -SeedDataFrom 'D:\wylde-seed-profile'

.NOTES
    Requires: git, cargo (Rust toolchain), and -- for a launch-verified run -- the
    host services the L3 legs probe (Ollama with the reasoner + embed models, and
    a Memgraph/Bolt endpoint with an indexed graph). CI structurally cannot run
    this (no GPU / Ollama / Memgraph in CI), so it runs on the release rig; the
    GREEN receipt is the deliverable, not a per-PR check.

    Windows PowerShell 5.1-safe, not just pwsh 7: ASCII-only so it parses
    regardless of encoding, AND free of pwsh-7-only inline if/switch
    expressions. An inline (if ...) as a command argument parses under 5.1 but
    throws at runtime there (5.1 treats it as a call to a command named 'if'),
    so status values are computed in preceding if-assignments instead; the
    switch uses here are '$x = switch' assignments, which are valid in 5.1. See
    issue #305 -- an earlier version false-REDed on the rig for exactly this.
#>
[CmdletBinding()]
param(
    [string] $Ref = 'origin/develop',
    [string] $RepoRoot,
    [string] $WorkDir,
    [string] $SeedDataFrom,
    [string] $HostLabel = 'automated-clean-install-preflight',
    [switch] $KeepWorkDir,
    [switch] $SkipTeardownCheck
)

$ErrorActionPreference = 'Stop'

# -- Resolve the source repo root ---------------------------------------------
if (-not $RepoRoot) {
    if ($PSScriptRoot) { $RepoRoot = Split-Path -Parent $PSScriptRoot }
    else { $RepoRoot = (Get-Location).Path }
}
$RepoRoot = (Resolve-Path -LiteralPath $RepoRoot).Path

function Invoke-Git {
    # Run git and throw on non-zero, returning trimmed stdout. Keeps native
    # stderr out of PowerShell's error stream (a 5.1 gotcha).
    param([string[]] $GitArgs, [string] $ErrContext = 'git')
    $out = & git @GitArgs 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "$ErrContext failed (exit $LASTEXITCODE): git $($GitArgs -join ' ')`n$out"
    }
    return ($out | Out-String).Trim()
}

# The repository to clone from: the common git dir's parent (the main worktree),
# so this is correct whether RepoRoot is a normal checkout or a linked worktree.
Push-Location $RepoRoot
try {
    $commonDir = Invoke-Git @('rev-parse', '--git-common-dir') 'git rev-parse --git-common-dir'
    $commonDirAbs = (Resolve-Path -LiteralPath $commonDir).Path
    $CloneSource = Split-Path -Parent $commonDirAbs
    # Pin the run to a single commit resolved in the source repo.
    $CommitSha = Invoke-Git @('rev-parse', $Ref) "git rev-parse $Ref"
} finally {
    Pop-Location
}

# -- Per-run scratch layout ---------------------------------------------------
if (-not $WorkDir) { $WorkDir = Join-Path $env:TEMP 'wylde-self-preflight' }
$RunId    = (Get-Date -Format 'yyyyMMdd-HHmmss') + '-' + $CommitSha.Substring(0, 8)
$RunDir   = Join-Path $WorkDir $RunId
$Checkout = Join-Path $RunDir 'checkout'
$DataDir  = Join-Path $RunDir 'clean-profile'
$Receipt  = Join-Path $RunDir 'preflight-receipt.json'

$results = New-Object System.Collections.ArrayList
function Add-Step {
    param([string] $Name, [string] $Status, [string] $Detail = '')
    [void]$results.Add([pscustomobject]@{ Step = $Name; Status = $Status; Detail = $Detail })
    $color = switch ($Status) { 'OK' { 'Green' } 'WARN' { 'Yellow' } default { 'Red' } }
    Write-Host ('  {0,-22} {1,-5} {2}' -f $Name, $Status, $Detail) -ForegroundColor $color
}

Write-Host ''
Write-Host '=== Wylde self-running clean-install preflight (#37) ===' -ForegroundColor Cyan
Write-Host ("  source repo : {0}" -f $CloneSource)
Write-Host ("  ref         : {0}  ({1})" -f $Ref, $CommitSha.Substring(0, 12))
Write-Host ("  run dir     : {0}" -f $RunDir)
Write-Host ("  data root   : {0}  (WYLDE_DATA_DIR -- clean profile)" -f $DataDir)
if ($SeedDataFrom) { Write-Host ("  seed from   : {0}" -f $SeedDataFrom) }
Write-Host ''

$exitCode = 1
try {
    # 1. Throwaway fresh checkout ---------------------------------------------
    New-Item -ItemType Directory -Path $RunDir -Force | Out-Null
    Invoke-Git @('clone', '--no-hardlinks', '--quiet', $CloneSource, $Checkout) 'git clone' | Out-Null
    Push-Location $Checkout
    try { Invoke-Git @('checkout', '--quiet', $CommitSha) "git checkout $CommitSha" | Out-Null }
    finally { Pop-Location }
    Add-Step 'fresh-checkout' 'OK' ("cloned @ " + $CommitSha.Substring(0, 12))

    # 2. Isolated, empty data root --------------------------------------------
    New-Item -ItemType Directory -Path $DataDir -Force | Out-Null
    if ($SeedDataFrom) {
        $seed = (Resolve-Path -LiteralPath $SeedDataFrom).Path
        Copy-Item -Path (Join-Path $seed '*') -Destination $DataDir -Recurse -Force
        Add-Step 'clean-profile' 'OK' 'seeded from reference profile'
    } else {
        Add-Step 'clean-profile' 'OK' 'empty (strict cold-start)'
    }

    # 3. Build + launch + assert ----------------------------------------------
    # WYLDE_DATA_DIR wins over the whole resolution chain, so the daemon and
    # every service it spawns read/write the clean profile. Set it only for the
    # child process's environment.
    $prevDataDir = $env:WYLDE_DATA_DIR
    $env:WYLDE_DATA_DIR = $DataDir
    $manifest = Join-Path $Checkout 'tools\wylde-release\Cargo.toml'
    $preflightArgs = @(
        'run', '--manifest-path', $manifest, '--',
        'preflight', '--launch', '--build',
        '--repo-root', $Checkout,
        '--host-label', $HostLabel,
        '--receipt', $Receipt
    )
    Write-Host ''
    Write-Host '--- wylde-release preflight --launch --build ---' -ForegroundColor Cyan
    # cargo streams compiler progress to stderr; under ErrorActionPreference=Stop
    # PowerShell 5.1 can turn that into a terminating error, so drop to Continue
    # around the native call (the same gotcha launch_wylde.ps1 handles) and gate
    # solely on the exit code.
    $prevEAP = $ErrorActionPreference
    $ErrorActionPreference = 'Continue'
    try {
        & cargo @preflightArgs
        $preflightExit = $LASTEXITCODE
    } finally {
        $ErrorActionPreference = $prevEAP
        # Restore our own environment regardless of outcome.
        if ($null -eq $prevDataDir) { Remove-Item Env:\WYLDE_DATA_DIR -ErrorAction SilentlyContinue }
        else { $env:WYLDE_DATA_DIR = $prevDataDir }
    }
    Write-Host '-----------------------------------------------' -ForegroundColor Cyan
    if ($preflightExit -eq 0) { Add-Step 'preflight-run' 'OK' 'wylde-release exited 0' }
    else { Add-Step 'preflight-run' 'FAIL' ("wylde-release exited " + $preflightExit) }

    # 4. Read + summarise the receipt -----------------------------------------
    if (Test-Path -LiteralPath $Receipt) {
        $r = Get-Content -LiteralPath $Receipt -Raw | ConvertFrom-Json
        # Each status is computed in a preceding `if` assignment, NOT passed
        # inline as an `(if ...)` argument. An inline if-expression is pwsh-7
        # only: under Windows PowerShell 5.1 it PARSES but throws at RUNTIME
        # (5.1 evaluates `(if ...)` as a call to a command named `if` and errors
        # "The term 'if' is not recognized ..."), which the try/catch below would
        # swallow into a false RED. Assignment-from-if is valid in both 5.1 and
        # pwsh 7 (#305).
        $commitStatus = if ($r.commit -like "$CommitSha*") { 'OK' } else { 'WARN' }
        Add-Step 'receipt.commit' $commitStatus $r.commit
        $dirtyStatus = if (-not $r.git_dirty) { 'OK' } else { 'WARN' }
        Add-Step 'receipt.dirty' $dirtyStatus ("git_dirty=" + $r.git_dirty)
        $allGreenStatus = if ($r.all_green) { 'OK' } else { 'FAIL' }
        Add-Step 'all_green' $allGreenStatus ''
        $launchStatus = if ($r.launch_verified) { 'OK' } else { 'FAIL' }
        Add-Step 'launch_verified' $launchStatus ''
        if ($r.gates) {
            foreach ($g in $r.gates.PSObject.Properties) {
                $st = switch ("$($g.Value)") { 'Pass' { 'OK' } 'Skipped' { 'WARN' } default { 'FAIL' } }
                Add-Step ("gate:" + $g.Name) $st ''
            }
        }
        Copy-Item -LiteralPath $Receipt -Destination (Join-Path $RunDir 'preflight-receipt.kept.json') -Force
        if ($r.all_green -and $r.launch_verified) { $exitCode = 0 } else { $exitCode = 2 }
    } else {
        Add-Step 'receipt' 'FAIL' 'no preflight-receipt.json produced'
        $exitCode = 3
    }
}
catch {
    Add-Step 'error' 'FAIL' ($_.Exception.Message)
    $exitCode = 1
}
finally {
    # 5. Teardown -- remove the throwaway tree + data root (the launched stack
    # is torn down by wylde-release itself). The receipt copy is kept in RunDir
    # only if -KeepWorkDir is set.
    if ($KeepWorkDir) {
        Write-Host ''
        Write-Host ("Kept run dir for inspection: {0}" -f $RunDir) -ForegroundColor Yellow
    } else {
        try {
            if (Test-Path -LiteralPath $RunDir) { Remove-Item -LiteralPath $RunDir -Recurse -Force -ErrorAction Stop }
            Write-Host ''
            Write-Host 'Torn down: throwaway checkout + clean profile removed.' -ForegroundColor DarkGray
        } catch {
            Write-Host ("WARNING: could not fully remove {0}: {1}" -f $RunDir, $_.Exception.Message) -ForegroundColor Yellow
        }
    }
}

# -- Summary ------------------------------------------------------------------
Write-Host ''
Write-Host '=== Summary ===' -ForegroundColor Cyan
$fail = @($results | Where-Object { $_.Status -eq 'FAIL' })
$warn = @($results | Where-Object { $_.Status -eq 'WARN' })
Write-Host ("  {0} steps, {1} FAIL, {2} WARN" -f $results.Count, $fail.Count, $warn.Count)
if ($exitCode -eq 0) {
    Write-Host '  RESULT: GREEN + LAUNCH-VERIFIED (clean-install preflight passed).' -ForegroundColor Green
} else {
    Write-Host ('  RESULT: NOT launch-verified (exit {0}). See the checks above.' -f $exitCode) -ForegroundColor Red
    if (-not $SeedDataFrom) {
        Write-Host '  NOTE: a strict-empty profile cannot pass l3.memgraph_has_data / l3.ollama_model.' -ForegroundColor DarkGray
        Write-Host '        Re-run with -SeedDataFrom <indexed profile> for a launch-verifiable run.' -ForegroundColor DarkGray
    }
}
Write-Host ''
exit $exitCode
