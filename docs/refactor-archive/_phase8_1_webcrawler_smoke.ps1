# Phase 8.1 Webcrawler smoke test — PowerShell wrapper.
#
# Run from a PowerShell prompt at the vault root, or right-click → Run
# with PowerShell. Calls the deep test runner at:
#   Wylde/Extensions/Webcrawler/tests/smoke_test.py
#
# Output is mirrored to _phase8_1_webcrawler_smoke_output.log next to
# this file so you can paste the log back to the assistant.

$ErrorActionPreference = "Stop"
Set-Location -Path $PSScriptRoot

$smokePy = Join-Path $PSScriptRoot "Extensions\Webcrawler\tests\smoke_test.py"
$logFile = Join-Path $PSScriptRoot "_phase8_1_webcrawler_smoke_output.log"

if (-not (Test-Path $smokePy)) {
    Write-Host "ERROR: cannot find $smokePy"
    Write-Host "The Phase 8.1 smoke test python is missing or has moved."
    exit 1
}

Write-Host "Running Phase 8.1 Webcrawler smoke test..."
Write-Host "Output will be saved to: $logFile"
Write-Host ""

# Capture both stdout and stderr to the log AND mirror to console.
& python $smokePy 2>&1 | Tee-Object -FilePath $logFile
$exitCode = $LASTEXITCODE

Write-Host ""
Write-Host "============================================================"
if ($exitCode -eq 0) {
    Write-Host "PASS — Phase 8.1 smoke test exited 0."
} else {
    Write-Host "FAIL — Phase 8.1 smoke test exited $exitCode."
    Write-Host "Full output saved to: $logFile"
}
Write-Host "============================================================"

exit $exitCode
