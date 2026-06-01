$ErrorActionPreference = 'Continue'
$logPath = Join-Path $PSScriptRoot '_led_diagnostic_output.log'
Start-Transcript -Path $logPath -Force | Out-Null

Write-Host "===================================================="
Write-Host " LED / RGB Crash Diagnostic"
Write-Host " Run: $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')"
Write-Host " Host: $env:COMPUTERNAME  User: $env:USERNAME"
Write-Host "===================================================="
Write-Host ""

$rgbHints = @('Aura','Armoury','RGB','LED','Fusion','Mystic','Mystic Light','iCUE','Corsair','Razer','NZXT','Lian Li','Phanteks','Cooler Master','Thermaltake','SignalRGB','OpenRGB')

# ---------- A ----------
Write-Host "=== A. Problem devices ==="
$probs = Get-PnpDevice -Status Error,Unknown -ErrorAction SilentlyContinue
if ($probs) {
    $probs | ForEach-Object {
        Write-Host "  [$($_.Status)] $($_.Class) | $($_.FriendlyName)"
        Write-Host "    InstanceId: $($_.InstanceId)"
    }
} else {
    Write-Host "  none"
}

# ---------- B ----------
Write-Host ""
Write-Host "=== B. RGB / chassis controller hardware ==="
$matches = Get-PnpDevice -ErrorAction SilentlyContinue | Where-Object {
    $name = $_.FriendlyName
    if (-not $name) { return $false }
    foreach ($h in $rgbHints) { if ($name -match $h) { return $true } }
    $false
}
if ($matches) {
    $matches | ForEach-Object {
        Write-Host "  [$($_.Status)] $($_.Class) | $($_.FriendlyName)"
        Write-Host "    InstanceId: $($_.InstanceId)"
    }
} else {
    Write-Host "  none -- could be header-only RGB with no exposed PnP device"
}

# ---------- C ----------
Write-Host ""
Write-Host "=== C. RGB software running right now ==="
$rgbProcs = @('AsusKBFiltrLdr','LightingService','OmenLightingService','ArmouryCrate','AuraService','AsusFanControlService','AsusOptimization','LightFX','RGBFusionService','MysticLight','MSIRGBManagement','iCUE','CorsairService','SynapseService','RzSynapse','SignalRgb','SignalRgbHelper','OpenRGB','CAM','aRGBMonitorService','LianLi','LLAUTOLED')
$found = Get-Process -ErrorAction SilentlyContinue | Where-Object {
    foreach ($p in $rgbProcs) { if ($_.Name -ieq $p -or $_.Name -match $p) { return $true } }
    $false
}
if ($found) {
    $found | Select-Object Name, Id, Path, Company | Format-Table -AutoSize | Out-String | Write-Host
} else {
    Write-Host "  no known RGB software process is currently running"
}

# ---------- D ----------
Write-Host ""
Write-Host "=== D. Startup entries (HKCU + HKLM Run + Startup folders) ==="
$startupKeys = @(
    'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Run',
    'HKLM:\SOFTWARE\WOW6432Node\Microsoft\Windows\CurrentVersion\Run',
    'HKCU:\SOFTWARE\Microsoft\Windows\CurrentVersion\Run'
)
foreach ($k in $startupKeys) {
    $key = Get-ItemProperty -Path $k -ErrorAction SilentlyContinue
    if ($key) {
        $key.PSObject.Properties | Where-Object {
            $val = $_.Value
            if (-not $val) { return $false }
            if ($_.Name -in @('PSPath','PSParentPath','PSChildName','PSDrive','PSProvider')) { return $false }
            $hit = $false
            foreach ($h in $rgbHints) { if ($val -match $h) { $hit = $true; break } }
            $hit
        } | ForEach-Object {
            Write-Host "  [$k] $($_.Name) = $($_.Value)"
        }
    }
}

$startupFolders = @(
    "$env:APPDATA\Microsoft\Windows\Start Menu\Programs\Startup",
    "$env:ProgramData\Microsoft\Windows\Start Menu\Programs\Startup"
)
foreach ($sf in $startupFolders) {
    if (Test-Path $sf) {
        Get-ChildItem -Path $sf -ErrorAction SilentlyContinue | Where-Object {
            $name = $_.Name
            $hit = $false
            foreach ($h in $rgbHints) { if ($name -match $h) { $hit = $true; break } }
            $hit
        } | ForEach-Object {
            Write-Host "  [$sf] $($_.Name)"
        }
    }
}

# ---------- E ----------
Write-Host ""
Write-Host "=== E. Driver installs in late March / April 2026 ==="
$cutoffStart = Get-Date "2026-03-25"
$cutoffEnd = Get-Date "2026-04-25"
try {
    $events = Get-WinEvent -FilterHashtable @{
        LogName='System'
        ProviderName='Microsoft-Windows-Kernel-PnP','Microsoft-Windows-DriverFrameworks-UserMode','Microsoft-Windows-WindowsUpdateClient'
        Level=4
        StartTime=$cutoffStart
        EndTime=$cutoffEnd
    } -ErrorAction Stop
} catch {
    Write-Host "  (no matching events or providers unavailable: $($_.Exception.Message))"
    $events = @()
}
$filtered = $events | Where-Object {
    $_.Message -match 'driver|installed' -and $_.Message -notmatch 'usbhub|HidUsb'
} | Select-Object -First 30
if ($filtered) {
    $filtered | ForEach-Object {
        Write-Host "  $($_.TimeCreated.ToString('MM-dd HH:mm:ss')) [$($_.ProviderName)] Id=$($_.Id)"
        $firstLine = ($_.Message -split "`n")[0]
        $cut = [Math]::Min(160, $firstLine.Length)
        Write-Host "    $($firstLine.Substring(0, $cut))"
    }
} else {
    Write-Host "  no matching driver/install events in window"
}

# ---------- F ----------
Write-Host ""
Write-Host "=== F. Summary ==="
$summary = @()

# RGB stack guess
$rgbStack = $null
if ($matches) {
    $rgbStack = ($matches | Select-Object -First 1).FriendlyName
    $summary += "Likely RGB stack hardware: $rgbStack"
} else {
    $summary += "Likely RGB stack hardware: none exposed as PnP device (header-only or USB-HID controller hidden behind generic class)"
}

# Software running
if ($found) {
    $summary += "RGB software running NOW: " + (($found | Select-Object -Expand Name -Unique) -join ', ')
} else {
    $summary += "RGB software running NOW: none of the known processes detected"
}

# Autostart
$autostartHits = @()
foreach ($k in $startupKeys) {
    $key = Get-ItemProperty -Path $k -ErrorAction SilentlyContinue
    if ($key) {
        $key.PSObject.Properties | ForEach-Object {
            $val = $_.Value
            if (-not $val) { return }
            if ($_.Name -in @('PSPath','PSParentPath','PSChildName','PSDrive','PSProvider')) { return }
            foreach ($h in $rgbHints) { if ($val -match $h) { $autostartHits += "$($_.Name)"; break } }
        }
    }
}
if ($autostartHits.Count -gt 0) {
    $summary += "RGB software autostarting: " + (($autostartHits | Select-Object -Unique) -join ', ')
} else {
    $summary += "RGB software autostarting: nothing matched"
}

# Suspicious in date range
if ($filtered) {
    $summary += "Driver events in 3/25-4/25 window: $($filtered.Count) hit(s) -- inspect section E"
} else {
    $summary += "Driver events in 3/25-4/25 window: none flagged"
}

foreach ($line in $summary) { Write-Host "  - $line" }

Write-Host ""
Write-Host "=== END ==="
Stop-Transcript | Out-Null
