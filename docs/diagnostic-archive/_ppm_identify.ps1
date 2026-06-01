# _ppm_identify.ps1 — gather hardware + Intel driver state for PPM diagnosis
# Output is teed to _ppm_identify_output.log next to this script.

$ErrorActionPreference = 'SilentlyContinue'
$scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$logPath   = Join-Path $scriptDir '_ppm_identify_output.log'

# Start a fresh log every run
Start-Transcript -Path $logPath -Force | Out-Null

Write-Host "PPM Provisioning Package — system identification"
Write-Host "Run at: $(Get-Date -Format 'yyyy-MM-dd HH:mm:ss')"
Write-Host ("=" * 72)

Write-Host "`n=== A. Motherboard ==="
Get-CimInstance Win32_BaseBoard | Select-Object Manufacturer, Product, Version | Format-List

Write-Host "`n=== B. CPU ==="
Get-CimInstance Win32_Processor | Select-Object Name, Manufacturer, NumberOfCores, NumberOfLogicalProcessors, MaxClockSpeed, ProcessorId | Format-List

Write-Host "`n=== C. BIOS ==="
Get-CimInstance Win32_BIOS | Select-Object Manufacturer, Name, Version, ReleaseDate, SMBIOSBIOSVersion | Format-List

Write-Host "`n=== D. Chipset ==="
Get-CimInstance Win32_PnPSignedDriver -ErrorAction SilentlyContinue |
    Where-Object { $_.DeviceClass -eq 'SYSTEM' -and $_.DeviceName -match 'Chipset|LPC|PCH|PMC|Provisioning' } |
    Select-Object DeviceName, DriverVersion, DriverDate, Manufacturer |
    Sort-Object DeviceName |
    Format-Table -AutoSize

Write-Host "`n=== E. PPM Provisioning Package status ==="
Get-PnpDevice -ErrorAction SilentlyContinue |
    Where-Object { $_.FriendlyName -match 'PPM Provisioning' -or $_.FriendlyName -match 'Intel.*Provisioning' } |
    Select-Object Status, FriendlyName, InstanceId, ConfigManagerErrorCode |
    Format-List

Write-Host "`n=== F. All Intel software components ==="
Get-PnpDevice -Class SoftwareComponent -ErrorAction SilentlyContinue |
    Where-Object { $_.FriendlyName -match 'Intel' } |
    Select-Object Status, FriendlyName, ConfigManagerErrorCode |
    Format-Table -AutoSize

Write-Host "`n=== G. Currently-installed Intel chipset / management driver ==="
Get-CimInstance Win32_PnPSignedDriver -ErrorAction SilentlyContinue |
    Where-Object { $_.Manufacturer -match 'Intel' -and $_.DeviceName -match 'Chipset|Management|MEI|DPTF|Dynamic Tuning|Power|Thermal' } |
    Select-Object DeviceName, DriverVersion, DriverDate |
    Sort-Object -Unique DeviceName |
    Format-Table -AutoSize

Write-Host "`n=== H. Last 5 PPM-related events ==="
Get-WinEvent -LogName 'System' -ErrorAction SilentlyContinue |
    Where-Object { $_.Message -match 'PPM|provisioning|Intel.*power' } |
    Select-Object -First 5 |
    ForEach-Object {
        Write-Host "  $($_.TimeCreated)  Id=$($_.Id)  $($_.LevelDisplayName)"
        Write-Host "    $(($_.Message -split "`n")[0])"
    }

Write-Host "`n=== I. Recent Kernel-Power 41 events (dirty shutdowns) ==="
Get-WinEvent -FilterHashtable @{LogName='System'; ProviderName='Microsoft-Windows-Kernel-Power'; Id=41} -MaxEvents 5 -ErrorAction SilentlyContinue |
    ForEach-Object {
        Write-Host "  $($_.TimeCreated)  Id=$($_.Id)  $($_.LevelDisplayName)"
    }

Write-Host "`n=== Done ==="
Write-Host "Log saved to: $logPath"

Stop-Transcript | Out-Null

Write-Host ""
Write-Host "Press Enter to close this window..." -ForegroundColor Yellow
Read-Host | Out-Null
