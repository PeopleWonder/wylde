$ErrorActionPreference = 'Continue'
$logPath = Join-Path $PSScriptRoot '_crash_investigate_output.log'

Start-Transcript -Path $logPath -Force | Out-Null

Write-Host "================================================================"
Write-Host " CRASH INVESTIGATION - $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')"
Write-Host " Target window: 2026-05-07 08:30 - 08:45"
Write-Host "================================================================"
Write-Host ""

# ---------------------------------------------------------------
# 1. Minidump files
# ---------------------------------------------------------------
Write-Host "=== 1. Minidump files (kernel BSOD dumps) ==="
$dumps = Get-ChildItem -Path "C:\Windows\Minidump\*.dmp" -ErrorAction SilentlyContinue |
         Where-Object { $_.LastWriteTime -gt (Get-Date).AddDays(-2) }
if ($dumps) {
    $dumps | ForEach-Object {
        Write-Host "  $($_.LastWriteTime.ToString('yyyy-MM-dd HH:mm:ss'))  $($_.Name)  $([math]::Round($_.Length / 1KB)) KB"
    }
} else {
    Write-Host "  none in last 2 days"
}

Write-Host ""
Write-Host "=== 1b. MEMORY.DMP (full kernel dump) ==="
$memdmp = Get-Item "C:\Windows\MEMORY.DMP" -ErrorAction SilentlyContinue
if ($memdmp) {
    Write-Host "  $($memdmp.LastWriteTime.ToString('yyyy-MM-dd HH:mm:ss'))  size=$([math]::Round($memdmp.Length / 1MB)) MB"
} else {
    Write-Host "  not present"
}
Write-Host ""

# ---------------------------------------------------------------
# 2. WHEA-Logger
# ---------------------------------------------------------------
Write-Host "=== 2. Hardware errors (WHEA-Logger, last 24h) ==="
$whea = Get-WinEvent -FilterHashtable @{
    LogName='System'
    ProviderName='Microsoft-Windows-WHEA-Logger'
    StartTime=(Get-Date).AddHours(-24)
} -ErrorAction SilentlyContinue
if ($whea) {
    $whea | ForEach-Object {
        Write-Host "  $($_.TimeCreated.ToString('HH:mm:ss')) Id=$($_.Id) Level=$($_.LevelDisplayName)"
        $msgLines = ($_.Message -split "`n")
        $preview = ($msgLines[0..([Math]::Min(2, $msgLines.Count - 1))] -join ' | ')
        Write-Host "    $preview"
    }
} else {
    Write-Host "  none"
}
Write-Host ""

# ---------------------------------------------------------------
# 3. Disk errors
# ---------------------------------------------------------------
Write-Host "=== 3. Disk errors (last 24h) ==="
$diskEvents = Get-WinEvent -FilterHashtable @{
    LogName='System'
    ProviderName='disk','Disk','volmgr','partmgr','Ntfs','Microsoft-Windows-Ntfs','storahci','stornvme'
    Level=1,2,3
    StartTime=(Get-Date).AddHours(-24)
} -ErrorAction SilentlyContinue
if ($diskEvents) {
    $diskEvents | ForEach-Object {
        Write-Host "  $($_.TimeCreated.ToString('HH:mm:ss')) [$($_.ProviderName)] Id=$($_.Id) Level=$($_.LevelDisplayName)"
        $first = ($_.Message -split "`n")[0]
        $first = $first.Substring(0, [Math]::Min(180, $first.Length))
        Write-Host "    $first"
    }
} else {
    Write-Host "  none"
}
Write-Host ""

# ---------------------------------------------------------------
# 4. System log around crash window
# ---------------------------------------------------------------
$crashWindow = Get-Date "2026-05-07 08:30:00"
$crashWindowEnd = Get-Date "2026-05-07 08:45:00"

Write-Host "=== 4. System log: events 08:30-08:45 today ==="
$sysWindow = Get-WinEvent -FilterHashtable @{
    LogName='System'
    StartTime=$crashWindow
    EndTime=$crashWindowEnd
} -ErrorAction SilentlyContinue | Sort-Object TimeCreated
if ($sysWindow) {
    $sysWindow | ForEach-Object {
        Write-Host "  $($_.TimeCreated.ToString('HH:mm:ss')) [$($_.LevelDisplayName.PadRight(11))] [$($_.ProviderName)] Id=$($_.Id)"
        $first = ($_.Message -split "`n")[0]
        $first = $first.Substring(0, [Math]::Min(180, $first.Length))
        Write-Host "    $first"
    }
} else {
    Write-Host "  no events in window"
}
Write-Host ""

# Also pull events 5 minutes before the crash window for boot/state context
Write-Host "=== 4b. System log: events 08:25-08:30 (just before window) ==="
$preWindow = Get-WinEvent -FilterHashtable @{
    LogName='System'
    StartTime=(Get-Date "2026-05-07 08:25:00")
    EndTime=$crashWindow
    Level=1,2,3
} -ErrorAction SilentlyContinue | Sort-Object TimeCreated
if ($preWindow) {
    $preWindow | ForEach-Object {
        Write-Host "  $($_.TimeCreated.ToString('HH:mm:ss')) [$($_.LevelDisplayName.PadRight(11))] [$($_.ProviderName)] Id=$($_.Id)"
        $first = ($_.Message -split "`n")[0]
        $first = $first.Substring(0, [Math]::Min(180, $first.Length))
        Write-Host "    $first"
    }
} else {
    Write-Host "  no error/warning events in 5min pre-window"
}
Write-Host ""

# ---------------------------------------------------------------
# 5. Application log around crash window
# ---------------------------------------------------------------
Write-Host "=== 5. Application log: events 08:30-08:45 today ==="
$appWindow = Get-WinEvent -FilterHashtable @{
    LogName='Application'
    StartTime=$crashWindow
    EndTime=$crashWindowEnd
} -ErrorAction SilentlyContinue | Sort-Object TimeCreated
$appHits = 0
if ($appWindow) {
    $appWindow | ForEach-Object {
        if ($_.LevelDisplayName -in 'Error','Critical','Warning') {
            $appHits++
            Write-Host "  $($_.TimeCreated.ToString('HH:mm:ss')) [$($_.LevelDisplayName.PadRight(11))] [$($_.ProviderName)] Id=$($_.Id)"
            $first = ($_.Message -split "`n")[0]
            $first = $first.Substring(0, [Math]::Min(180, $first.Length))
            Write-Host "    $first"
        }
    }
}
if ($appHits -eq 0) {
    Write-Host "  no error/critical/warning events in window"
}
Write-Host ""

# ---------------------------------------------------------------
# 6. Reliability Monitor
# ---------------------------------------------------------------
Write-Host "=== 6. Reliability Monitor (last 2 days, top 20) ==="
$relRecords = Get-CimInstance -ClassName Win32_ReliabilityRecords -ErrorAction SilentlyContinue |
  Where-Object { $_.TimeGenerated -gt (Get-Date).AddDays(-2) } |
  Sort-Object TimeGenerated -Descending |
  Select-Object -First 20
if ($relRecords) {
    $relRecords | ForEach-Object {
        Write-Host "  $($_.TimeGenerated)  [$($_.SourceName)]  $($_.ProductName)"
        if ($_.Message) {
            $msg = $_.Message.Substring(0, [Math]::Min(150, $_.Message.Length))
            Write-Host "    $msg"
        }
    }
} else {
    Write-Host "  no reliability records found"
}
Write-Host ""

# ---------------------------------------------------------------
# 7. Windows Error Reporting
# ---------------------------------------------------------------
Write-Host "=== 7. Windows Error Reporting reports (last 2 days) ==="
$werPaths = @(
    "$env:LOCALAPPDATA\Microsoft\Windows\WER\ReportArchive",
    "$env:LOCALAPPDATA\Microsoft\Windows\WER\ReportQueue",
    "$env:ProgramData\Microsoft\Windows\WER\ReportArchive",
    "$env:ProgramData\Microsoft\Windows\WER\ReportQueue"
)
$werFound = 0
foreach ($p in $werPaths) {
    if (Test-Path $p) {
        $reports = Get-ChildItem -Path $p -Directory -ErrorAction SilentlyContinue |
                   Where-Object { $_.LastWriteTime -gt (Get-Date).AddDays(-2) }
        foreach ($r in $reports) {
            $werFound++
            Write-Host "  $($r.LastWriteTime.ToString('yyyy-MM-dd HH:mm:ss'))  $($r.FullName)"
            $reportFile = Get-ChildItem -Path $r.FullName -Filter "Report.wer" -ErrorAction SilentlyContinue | Select-Object -First 1
            if ($reportFile) {
                $content = Get-Content $reportFile.FullName -ErrorAction SilentlyContinue
                $eventName = ($content | Where-Object { $_ -match '^EventName=' }) -replace 'EventName=',''
                $appName = ($content | Where-Object { $_ -match '^AppName=' }) -replace 'AppName=',''
                $sig0 = ($content | Where-Object { $_ -match '^Sig\[0\]\.Value=' } | Select-Object -First 1)
                $sig1 = ($content | Where-Object { $_ -match '^Sig\[1\]\.Value=' } | Select-Object -First 1)
                if ($eventName) { Write-Host "    EventName: $eventName" }
                if ($appName)   { Write-Host "    AppName:   $appName" }
                if ($sig0)      { Write-Host "    $sig0" }
                if ($sig1)      { Write-Host "    $sig1" }
            }
        }
    }
}
if ($werFound -eq 0) {
    Write-Host "  no WER reports in last 2 days"
}
Write-Host ""

# ---------------------------------------------------------------
# 8. Thermal / processor power events
# ---------------------------------------------------------------
Write-Host "=== 8. Thermal / kernel-power / processor-power events (last 24h) ==="
$thermal = Get-WinEvent -FilterHashtable @{
    LogName='System'
    ProviderName='Microsoft-Windows-Kernel-Processor-Power','Microsoft-Windows-Kernel-Power','Microsoft-Windows-Thermal-Service'
    StartTime=(Get-Date).AddHours(-24)
} -ErrorAction SilentlyContinue | Sort-Object TimeCreated
if ($thermal) {
    $thermal | ForEach-Object {
        Write-Host "  $($_.TimeCreated.ToString('HH:mm:ss')) [$($_.ProviderName)] Id=$($_.Id) Level=$($_.LevelDisplayName)"
    }
} else {
    Write-Host "  none"
}
Write-Host ""

# ---------------------------------------------------------------
# 9. Recurrence check - prior dirty shutdowns
# ---------------------------------------------------------------
Write-Host "=== 9. Prior dirty shutdowns (Kernel-Power 41 + EventLog 6008, last 30 days) ==="
$dirty41 = Get-WinEvent -FilterHashtable @{
    LogName='System'
    ProviderName='Microsoft-Windows-Kernel-Power'
    Id=41
    StartTime=(Get-Date).AddDays(-30)
} -ErrorAction SilentlyContinue
$dirty6008 = Get-WinEvent -FilterHashtable @{
    LogName='System'
    ProviderName='EventLog'
    Id=6008
    StartTime=(Get-Date).AddDays(-30)
} -ErrorAction SilentlyContinue
Write-Host "  Kernel-Power 41 events:"
if ($dirty41) {
    $dirty41 | Sort-Object TimeCreated | ForEach-Object {
        $bcc = ''
        $bcp1 = ''
        try {
            $props = $_.Properties
            if ($props.Count -ge 5) {
                $bcc = $props[4].Value
                $bcp1 = if ($props.Count -ge 6) { $props[5].Value } else { '' }
            }
        } catch {}
        Write-Host "    $($_.TimeCreated.ToString('yyyy-MM-dd HH:mm:ss'))  BugcheckCode=$bcc  BugcheckParameter1=$bcp1"
    }
} else {
    Write-Host "    none"
}
Write-Host "  EventLog 6008 events:"
if ($dirty6008) {
    $dirty6008 | Sort-Object TimeCreated | ForEach-Object {
        Write-Host "    $($_.TimeCreated.ToString('yyyy-MM-dd HH:mm:ss'))"
    }
} else {
    Write-Host "    none"
}
Write-Host ""

# ---------------------------------------------------------------
# 10. Bugcheck details from this morning's Kernel-Power 41
# ---------------------------------------------------------------
Write-Host "=== 10. Today's Kernel-Power 41 - full details ==="
$todayKP = Get-WinEvent -FilterHashtable @{
    LogName='System'
    ProviderName='Microsoft-Windows-Kernel-Power'
    Id=41
    StartTime=(Get-Date "2026-05-07 00:00:00")
    EndTime=(Get-Date "2026-05-07 23:59:59")
} -ErrorAction SilentlyContinue
if ($todayKP) {
    $todayKP | ForEach-Object {
        Write-Host "  TimeCreated: $($_.TimeCreated)"
        Write-Host "  Message:"
        ($_.Message -split "`n") | ForEach-Object { Write-Host "    $_" }
        Write-Host "  Properties:"
        for ($i = 0; $i -lt $_.Properties.Count; $i++) {
            Write-Host "    [$i] $($_.Properties[$i].Value)"
        }
    }
} else {
    Write-Host "  none today"
}
Write-Host ""

# ---------------------------------------------------------------
# Summary heuristic
# ---------------------------------------------------------------
Write-Host "================================================================"
Write-Host " HEURISTIC SUMMARY"
Write-Host "================================================================"

$hasMinidump = ($dumps -and ($dumps | Where-Object { $_.LastWriteTime -gt (Get-Date "2026-05-07 00:00:00") }))
$hasWhea     = ($whea  -and ($whea  | Where-Object { $_.TimeCreated -gt (Get-Date "2026-05-07 00:00:00") }))
$hasDisk     = ($diskEvents -and ($diskEvents | Where-Object { $_.TimeCreated -gt (Get-Date "2026-05-07 00:00:00") }))
$hasMemDmp   = ($memdmp -and $memdmp.LastWriteTime -gt (Get-Date "2026-05-07 00:00:00"))

Write-Host "  Minidump from today?:    $([bool]$hasMinidump)"
Write-Host "  MEMORY.DMP from today?:  $([bool]$hasMemDmp)"
Write-Host "  WHEA hits today?:        $([bool]$hasWhea)"
Write-Host "  Disk errors today?:      $([bool]$hasDisk)"
Write-Host "  KP41 events today:       $(@($todayKP).Count)"
Write-Host ""

if ($hasWhea) {
    Write-Host "  -> Hardware fault signal detected (WHEA). Check section 2 for details."
} elseif ($hasMinidump -or $hasMemDmp) {
    Write-Host "  -> BSOD/kernel dump exists. Open in WinDbg with !analyze -v to get the bug check."
} elseif ($hasDisk) {
    Write-Host "  -> Storage subsystem errors near crash. Check section 3."
} else {
    Write-Host "  -> Only KP41/6008 with no other signal: likely power loss, hard freeze,"
    Write-Host "     manual reset, PSU issue, or hardware lockup that didn't bug-check."
}

Write-Host ""
Write-Host "================================================================"
Write-Host " END - $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')"
Write-Host "================================================================"

Stop-Transcript | Out-Null
