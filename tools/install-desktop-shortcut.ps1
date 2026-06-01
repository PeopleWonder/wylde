<#
.SYNOPSIS
    Audit every place a Wylde shortcut could live, remove duplicates, and create
    a single fresh Desktop shortcut to the integrated wylde-gui.exe.

.DESCRIPTION
    Step 1  Audits Desktop / Public Desktop / Start Menu (user + all-users) /
            Startup folders for any .lnk whose Target/Arguments/WorkingDirectory
            mention "wylde" or the old Tauri tree ("src-tauri").
    Step 2  Prints every match (path + resolved target) BEFORE touching anything.
    Step 3  Prompts y/n (Read-Host) so you stay in control of the deletion.
    Step 4  On confirm: deletes every writable match, then creates one fresh
            shortcut at <Desktop>\Wylde.lnk pointing at the canonical binary.
    Step 5  Icon = bundled Core/GUI/assets/icons/icon.ico if present, else the
            binary's own embedded icon.
    Step 6  WorkingDirectory = the binary's parent (launcher resolves relative
            asset/manifest paths from there).
    Step 7  Reports the final shortcut path + target.

    Idempotent: re-running finds the single Wylde.lnk it created, offers to
    replace it, and leaves exactly one shortcut. All-users locations that need
    admin are SKIPPED on deletion (reported, not failed) so no UAC is required.

    ASCII-only on purpose so it parses under Windows PowerShell 5.1 regardless of
    file encoding, whether run as a file or pasted inline.

.PARAMETER FindOnly
    Run steps 1-2 only (audit + print), then exit. No prompt, no delete, no
    create. Safe to paste anywhere to see what exists.

.PARAMETER RepoRoot
    Wylde repo root. Defaults to the parent of this script's folder, or the
    current directory if the script is pasted inline.

.EXAMPLE
    pwsh -NoProfile -File tools\install-desktop-shortcut.ps1 -FindOnly
.EXAMPLE
    powershell -ExecutionPolicy Bypass -File tools\install-desktop-shortcut.ps1
#>
[CmdletBinding()]
param(
    [switch] $FindOnly,
    [string] $RepoRoot
)

$ErrorActionPreference = 'Stop'

# -- Resolve repo root --------------------------------------------------------
if (-not $RepoRoot) {
    if ($PSScriptRoot) {
        # tools\install-desktop-shortcut.ps1 -> repo root is the parent of tools\
        $RepoRoot = Split-Path -Parent $PSScriptRoot
    }
    else {
        $RepoRoot = (Get-Location).Path
    }
}
$RepoRoot = (Resolve-Path -LiteralPath $RepoRoot).Path
Write-Host "Wylde repo root : $RepoRoot" -ForegroundColor Cyan

# -- Locate the canonical wylde-gui.exe ---------------------------------------
# The GPUI build lives in the standalone Core/GUI/ workspace (NOT rust/target),
# which is why it is not under rust/target/release. We try the known-good path
# first, then the historical guess, then a bounded recursive fallback.
$binCandidates = @(
    (Join-Path $RepoRoot 'Core\GUI\target\release\wylde-gui.exe'),
    (Join-Path $RepoRoot 'rust\target\release\wylde-gui.exe')
)
$binPath = $binCandidates | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1

if (-not $binPath) {
    Write-Host "wylde-gui.exe not at either known path; scanning target dirs..." -ForegroundColor Yellow
    foreach ($base in @((Join-Path $RepoRoot 'Core\GUI\target'), (Join-Path $RepoRoot 'rust\target'))) {
        if (Test-Path -LiteralPath $base) {
            $hit = Get-ChildItem -LiteralPath $base -Recurse -Filter 'wylde-gui.exe' -File -ErrorAction SilentlyContinue |
                   Select-Object -First 1
            if ($hit) { $binPath = $hit.FullName; break }
        }
    }
}

if (-not $FindOnly -and -not $binPath) {
    throw "Could not find wylde-gui.exe under $RepoRoot. Build it (Core/GUI/) before installing the shortcut."
}
if ($binPath) {
    Write-Host "Target binary   : $binPath" -ForegroundColor Cyan
    $binDir = Split-Path -Parent $binPath
}

# -- Locate the bundled icon --------------------------------------------------
$iconCandidates = @(
    (Join-Path $RepoRoot 'Core\GUI\assets\icons\icon.ico'),
    (Join-Path $RepoRoot 'assets\icon.ico'),
    (Join-Path $RepoRoot 'assets\icons\icon.ico'),
    (Join-Path $RepoRoot 'rust\crates\wylde-gui\icon.ico')
)
$iconPath = $iconCandidates | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1

# -- Build the list of scan roots ---------------------------------------------
# isAllUsers = needs admin to delete from; we report but skip those.
function New-ScanRoot {
    param([string] $Name, [string] $Folder, [bool] $AllUsers)
    if (-not $Folder) { return $null }
    if (-not (Test-Path -LiteralPath $Folder)) { return $null }
    [pscustomobject]@{
        Name     = $Name
        Path     = (Resolve-Path -LiteralPath $Folder).Path
        AllUsers = $AllUsers
    }
}

$candidateRoots = @(
    (New-ScanRoot 'Desktop'              ([Environment]::GetFolderPath('Desktop'))                 $false),
    (New-ScanRoot 'DesktopDirectory'     ([Environment]::GetFolderPath('DesktopDirectory'))        $false),
    (New-ScanRoot 'Public Desktop'       ([Environment]::GetFolderPath('CommonDesktopDirectory'))  $true),
    (New-ScanRoot 'Start Menu (user)'    ([Environment]::GetFolderPath('StartMenu'))               $false),
    (New-ScanRoot 'Programs (user)'      ([Environment]::GetFolderPath('Programs'))                $false),
    (New-ScanRoot 'Programs (all users)' ([Environment]::GetFolderPath('CommonPrograms'))          $true),
    (New-ScanRoot 'Startup (user)'       ([Environment]::GetFolderPath('Startup'))                 $false),
    (New-ScanRoot 'Startup (all users)'  ([Environment]::GetFolderPath('CommonStartup'))           $true)
) | Where-Object { $_ -ne $null }

# Some all-users Start Menu roots are only exposed via CommonStartMenu; add it
# defensively (GetFolderPath returns '' on platforms that lack it -> filtered).
$extra = New-ScanRoot 'Start Menu (all users)' ([Environment]::GetFolderPath('CommonStartMenu')) $true
if ($extra) { $candidateRoots += $extra }

# De-dupe by resolved path (Desktop and DesktopDirectory are usually identical;
# Programs is nested under StartMenu, so a recursive scan of StartMenu would
# double-count -- keep the shallowest unique path).
$seen = @{}
$scanRoots = @()
foreach ($r in ($candidateRoots | Sort-Object { $_.Path.Length })) {
    $key = $r.Path.ToLowerInvariant()
    $isNested = $false
    foreach ($existing in $scanRoots) {
        if ($key.StartsWith($existing.Path.ToLowerInvariant() + [IO.Path]::DirectorySeparatorChar)) {
            $isNested = $true; break
        }
    }
    if (-not $seen.ContainsKey($key) -and -not $isNested) {
        $seen[$key] = $true
        $scanRoots += $r
    }
}

# -- Scan for existing Wylde shortcuts ----------------------------------------
$shell = New-Object -ComObject WScript.Shell
$keywords = @('wylde', 'src-tauri')   # case-insensitive; covers wylde-gui / wylde-launcher / old Tauri tree

function Test-IsWyldeShortcut {
    param([string] $LnkPath)
    try {
        $sc = $shell.CreateShortcut($LnkPath)
        $hay = (@($LnkPath, $sc.TargetPath, $sc.Arguments, $sc.WorkingDirectory, $sc.IconLocation) -join ' ').ToLowerInvariant()
        foreach ($kw in $keywords) { if ($hay.Contains($kw)) { return $sc } }
    }
    catch { return $null }
    return $null
}

$found = @()
foreach ($root in $scanRoots) {
    $lnks = Get-ChildItem -LiteralPath $root.Path -Recurse -Filter '*.lnk' -File -ErrorAction SilentlyContinue
    foreach ($lnk in $lnks) {
        $sc = Test-IsWyldeShortcut -LnkPath $lnk.FullName
        if ($sc) {
            # Classify: a "launcher" shortcut is one we are replacing (points at
            # wylde-gui or an old launch_wylde script). Anything else that merely
            # mentions Wylde (e.g. the WyldeLink WireGuard forwarder in Startup)
            # is flagged "other" so it is not deleted by accident.
            $hay = (@($lnk.FullName, $sc.TargetPath, $sc.Arguments, $sc.WorkingDirectory) -join ' ').ToLowerInvariant()
            $kind = if ($hay -match 'wylde-gui|launch_wylde|src-tauri') { 'launcher' } else { 'other' }
            $found += [pscustomobject]@{
                RootName   = $root.Name
                AllUsers   = $root.AllUsers
                Path       = $lnk.FullName
                Target     = $sc.TargetPath
                Arguments  = $sc.Arguments
                WorkingDir = $sc.WorkingDirectory
                Kind       = $kind
            }
        }
    }
}

# -- Step 2: report findings BEFORE touching anything -------------------------
Write-Host ""
Write-Host "=== Scanned locations ===" -ForegroundColor Cyan
foreach ($root in $scanRoots) {
    $tag = if ($root.AllUsers) { ' [all-users]' } else { '' }
    Write-Host ("  {0,-24}{1}{2}" -f $root.Name, $root.Path, $tag)
}

Write-Host ""
Write-Host "=== Existing Wylde shortcuts found ($($found.Count)) ===" -ForegroundColor Cyan
if ($found.Count -eq 0) {
    Write-Host "  (none)" -ForegroundColor Green
}
else {
    foreach ($m in $found) {
        $tag = if ($m.AllUsers) { ' [all-users -> needs admin to delete]' } else { '' }
        $kindTag = if ($m.Kind -eq 'launcher') { '[launcher - replace]' } else { '[other - KEEP unless you know it is stale]' }
        $kindColor = if ($m.Kind -eq 'launcher') { 'Yellow' } else { 'Magenta' }
        Write-Host ("  LNK : {0}{1}  {2}" -f $m.Path, $tag, $kindTag) -ForegroundColor $kindColor
        Write-Host ("        -> {0} {1}" -f $m.Target, $m.Arguments)
    }
    if ($found | Where-Object { $_.Kind -eq 'other' }) {
        Write-Host "  NOTE: '[other]' matches mention Wylde but are not GUI launchers" -ForegroundColor Magenta
        Write-Host "        (e.g. the WyldeLink WireGuard Startup forwarder). Keep them unless stale." -ForegroundColor Magenta
    }
}

if ($iconPath) { Write-Host "`nIcon to apply   : $iconPath" -ForegroundColor Cyan }
else           { Write-Host "`nIcon to apply   : (binary embedded icon)" -ForegroundColor Cyan }

$desktop  = [Environment]::GetFolderPath('Desktop')
$freshLnk = Join-Path $desktop 'Wylde.lnk'
Write-Host "Fresh shortcut  : $freshLnk" -ForegroundColor Cyan

if ($FindOnly) {
    Write-Host "`n[FindOnly] No changes made." -ForegroundColor Green
    return
}

# -- Step 3: prompt -----------------------------------------------------------
# Per the find on this machine, matches can include non-launcher Wylde shortcuts
# (e.g. the WyldeLink WireGuard Startup forwarder). A blanket delete-all would
# nuke those, so offer all / each / none. "each" prompts y/n per shortcut.
$deleted = @()
$skipped = @()
$declined = @()

if ($found.Count -gt 0) {
    Write-Host ""
    $mode = Read-Host "Delete shortcuts before creating the fresh one?  [a]=all writable  [e]=choose each  [n]=none"
    if ($mode -notmatch '^(a|all|e|each)$') {
        Write-Host "Keeping all existing shortcuts; will still (re)create the fresh Desktop shortcut." -ForegroundColor Yellow
    }
    else {
        foreach ($m in $found) {
            $doDelete = $true
            if ($mode -match '^(e|each)$') {
                $kindHint = if ($m.Kind -eq 'other') { ' (NOT a launcher - probably keep)' } else { '' }
                $ans = Read-Host ("Delete '{0}'{1}? (y/n)" -f $m.Path, $kindHint)
                $doDelete = $ans -match '^(y|yes)$'
            }
            if (-not $doDelete) { $declined += $m.Path; continue }
            try {
                Remove-Item -LiteralPath $m.Path -Force -ErrorAction Stop
                $deleted += $m.Path
            }
            catch {
                $skipped += $m.Path
                Write-Host "  SKIPPED (not writable / needs admin): $($m.Path)" -ForegroundColor DarkYellow
            }
        }
    }
}

# -- Step 4b/5/6: create the fresh shortcut -----------------------------------
$lnk = $shell.CreateShortcut($freshLnk)
$lnk.TargetPath       = $binPath
$lnk.WorkingDirectory = $binDir
$lnk.Description      = 'Wylde - local AI control panel'
if ($iconPath) { $lnk.IconLocation = "$iconPath,0" }
else           { $lnk.IconLocation = "$binPath,0" }
$lnk.Save()

# -- Step 7: report -----------------------------------------------------------
Write-Host ""
Write-Host "=== Done ===" -ForegroundColor Green
Write-Host ("  Deleted : {0}" -f $deleted.Count)
if ($declined.Count -gt 0) {
    Write-Host ("  Kept    : {0} (you chose not to delete)" -f $declined.Count)
    $declined | ForEach-Object { Write-Host "            $_" }
}
if ($skipped.Count -gt 0) {
    Write-Host ("  Skipped : {0} (all-users / needs admin - rerun elevated to clear)" -f $skipped.Count) -ForegroundColor DarkYellow
    $skipped | ForEach-Object { Write-Host "            $_" -ForegroundColor DarkYellow }
}
Write-Host ("  Created : {0}" -f $freshLnk) -ForegroundColor Green
Write-Host ("  Target  : {0}" -f $binPath)
Write-Host ("  WorkDir : {0}" -f $binDir)
$iconShown = if ($iconPath) { $iconPath } else { "$binPath (embedded)" }
Write-Host ("  Icon    : {0}" -f $iconShown)
