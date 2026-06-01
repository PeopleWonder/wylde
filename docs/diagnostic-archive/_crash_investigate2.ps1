$ErrorActionPreference = 'Continue'
$logPath = Join-Path $PSScriptRoot '_crash_investigate2_output.log'
Start-Transcript -Path $logPath -Force | Out-Null

Write-Host "=== Reading WER queue Report.wer files (top 5 most recent) ==="
$queue = "C:\ProgramData\Microsoft\Windows\WER\ReportQueue"
$dirs = Get-ChildItem -Path $queue -Directory -ErrorAction SilentlyContinue |
        Where-Object { $_.Name -like "Kernel_*" } |
        Sort-Object LastWriteTime -Descending | Select-Object -First 8

foreach ($d in $dirs) {
    Write-Host "---- $($d.Name)  (Last write: $($d.LastWriteTime)) ----"
    $rep = Join-Path $d.FullName "Report.wer"
    if (Test-Path $rep) {
        $content = Get-Content $rep -ErrorAction SilentlyContinue
        # Print all interesting lines
        $content | Where-Object {
            $_ -match '^(EventName|EventTime|UI\[2\]|Sig\[\d\]\.Name|Sig\[\d\]\.Value|DynamicSig\[\d\]\.Name|DynamicSig\[\d\]\.Value|FriendlyEventName|AppName|Response\.|FileVersion)='
        } | ForEach-Object { Write-Host "  $_" }
    } else {
        Write-Host "  (no Report.wer)"
    }
    Write-Host ""
}

Write-Host ""
Write-Host "=== Specifically: any WER queue items with EventTime around 08:40 today ==="
# EventTime field is FILETIME format (100ns ticks since 1601). We look at LastWriteTime as a proxy.
$allDirs = Get-ChildItem -Path $queue -Directory -ErrorAction SilentlyContinue
foreach ($d in $allDirs) {
    $rep = Join-Path $d.FullName "Report.wer"
    if (Test-Path $rep) {
        $eventTimeLine = (Get-Content $rep -ErrorAction SilentlyContinue | Where-Object { $_ -match '^EventTime=' } | Select-Object -First 1)
        if ($eventTimeLine) {
            $ticks = [int64]($eventTimeLine -replace 'EventTime=','')
            try {
                $dt = [DateTime]::FromFileTime($ticks)
                if ($dt.Date -eq (Get-Date "2026-05-07").Date) {
                    Write-Host "  $($d.Name)"
                    Write-Host "    EventTime: $dt"
                    Get-Content $rep -ErrorAction SilentlyContinue | Where-Object {
                        $_ -match '^(EventName|Sig\[\d\]\.Name|Sig\[\d\]\.Value|DynamicSig\[\d\]\.Name|DynamicSig\[\d\]\.Value)='
                    } | ForEach-Object { Write-Host "      $_" }
                }
            } catch {}
        }
    }
}

Write-Host ""
Write-Host "=== Sleep/wake events on 5/7 (full timeline) ==="
Get-WinEvent -FilterHashtable @{
    LogName='System'
    ProviderName='Microsoft-Windows-Kernel-Power','Microsoft-Windows-Power-Troubleshooter'
    Id=1,42,107,109,131,506,507
    StartTime=(Get-Date "2026-05-07 00:00:00")
    EndTime=(Get-Date "2026-05-07 23:59:59")
} -ErrorAction SilentlyContinue | Sort-Object TimeCreated | ForEach-Object {
    Write-Host "  $($_.TimeCreated.ToString('HH:mm:ss')) [$($_.ProviderName)] Id=$($_.Id)"
    $first = ($_.Message -split "`n")[0]
    Write-Host "    $($first.Substring(0, [Math]::Min(150, $first.Length)))"
}

Write-Host ""
Write-Host "=== Last 5 system-log events BEFORE 08:40:38 today ==="
Get-WinEvent -FilterHashtable @{
    LogName='System'
    StartTime=(Get-Date "2026-05-07 00:00:00")
    EndTime=(Get-Date "2026-05-07 08:40:38")
} -MaxEvents 5 -ErrorAction SilentlyContinue | ForEach-Object {
    Write-Host "  $($_.TimeCreated.ToString('HH:mm:ss')) [$($_.LevelDisplayName)] [$($_.ProviderName)] Id=$($_.Id)"
    $first = ($_.Message -split "`n")[0]
    Write-Host "    $($first.Substring(0, [Math]::Min(150, $first.Length)))"
}

Write-Host ""
Write-Host "=== First 5 system-log events AFTER 18:59 (post-recovery) ==="
Get-WinEvent -FilterHashtable @{
    LogName='System'
    StartTime=(Get-Date "2026-05-07 18:59:00")
    EndTime=(Get-Date "2026-05-07 19:01:00")
} -ErrorAction SilentlyContinue | Sort-Object TimeCreated | Select-Object -First 10 | ForEach-Object {
    Write-Host "  $($_.TimeCreated.ToString('HH:mm:ss')) [$($_.LevelDisplayName)] [$($_.ProviderName)] Id=$($_.Id)"
    $first = ($_.Message -split "`n")[0]
    Write-Host "    $($first.Substring(0, [Math]::Min(150, $first.Length)))"
}

Write-Host ""
Write-Host "=== Power dump-config status ==="
$ck = Get-ItemProperty "HKLM:\SYSTEM\CurrentControlSet\Control\CrashControl" -ErrorAction SilentlyContinue
if ($ck) {
    Write-Host "  CrashDumpEnabled: $($ck.CrashDumpEnabled)  (0=none, 1=full, 2=kernel, 3=small/mini, 7=automatic)"
    Write-Host "  DumpFile:         $($ck.DumpFile)"
    Write-Host "  MinidumpDir:      $($ck.MinidumpDir)"
    Write-Host "  AutoReboot:       $($ck.AutoReboot)"
    Write-Host "  Overwrite:        $($ck.Overwrite)"
} else {
    Write-Host "  CrashControl key not readable"
}

Stop-Transcript | Out-Null
