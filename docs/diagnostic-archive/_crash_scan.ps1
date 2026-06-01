$ErrorActionPreference = 'Stop'
$start = (Get-Date).AddHours(-24)

Write-Host "=== Windows crash scan ($(Get-Date)) ==="
Write-Host "Window: $start to $(Get-Date)"
Write-Host ""

# Application errors / unhandled exceptions / WER reports
Write-Host "--- Application Log: errors and critical events ---"
$appEvents = Get-WinEvent -FilterHashtable @{
    LogName='Application'
    Level=1,2  # Critical, Error
    StartTime=$start
} -ErrorAction SilentlyContinue

$crashSources = @('Application Error','Windows Error Reporting','.NET Runtime','Application Hang','SideBySide')
$relevantApp = $appEvents | Where-Object { $crashSources -contains $_.ProviderName }

if ($relevantApp) {
    $relevantApp | Group-Object ProviderName | ForEach-Object {
        Write-Host ""
        Write-Host "[$($_.Name)] - $($_.Count) event(s)"
        $_.Group | Select-Object -First 5 | ForEach-Object {
            Write-Host "  $($_.TimeCreated.ToString('HH:mm:ss')) Id=$($_.Id) - $(($_.Message -split "`n")[0].Substring(0, [Math]::Min(150, ($_.Message -split "`n")[0].Length)))"
        }
        if ($_.Count -gt 5) { Write-Host "  ... and $($_.Count - 5) more" }
    }
} else {
    Write-Host "  none"
}

Write-Host ""
Write-Host "--- System Log: BugCheck (BSOD) and unexpected shutdowns ---"
$sysEvents = Get-WinEvent -FilterHashtable @{
    LogName='System'
    Level=1,2,3
    StartTime=$start
} -ErrorAction SilentlyContinue | Where-Object {
    $_.Id -in 41,1001,6008,6005,6006,6013 -or  # Kernel-Power, BugCheck, EventLog start/stop
    $_.ProviderName -eq 'Microsoft-Windows-WER-SystemErrorReporting'
}

if ($sysEvents) {
    $sysEvents | ForEach-Object {
        Write-Host "  $($_.TimeCreated.ToString('HH:mm:ss')) [$($_.ProviderName)] Id=$($_.Id) Level=$($_.LevelDisplayName)"
        Write-Host "    $(($_.Message -split "`n")[0].Substring(0, [Math]::Min(200, ($_.Message -split "`n")[0].Length)))"
    }
} else {
    Write-Host "  none"
}

Write-Host ""
Write-Host "--- Summary ---"
Write-Host "Application errors:  $($relevantApp.Count)"
Write-Host "System errors/BSOD:  $($sysEvents.Count)"
Write-Host "Total crash-shaped events in 24h: $($relevantApp.Count + $sysEvents.Count)"
