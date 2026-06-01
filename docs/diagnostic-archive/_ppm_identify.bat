@echo off
REM _ppm_identify.bat — double-click from File Explorer to run.
REM Self-elevates so PnP/WMI queries return complete data.

setlocal
cd /d "%~dp0"

REM --- Self-elevate to admin ---
net session >nul 2>&1
if %errorLevel% neq 0 (
    echo Requesting administrator access...
    powershell -NoProfile -Command "Start-Process '%~f0' -Verb RunAs"
    exit /b
)

echo ==========================================================
echo  PPM Provisioning Package - System Identification
echo ==========================================================
echo  This will gather hardware + Intel driver info and write
echo  the result to:
echo     %~dp0_ppm_identify_output.log
echo.
echo  Output also displayed below. No changes will be made.
echo ==========================================================
echo.

powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0_ppm_identify.ps1"

echo.
echo Done. Log written to _ppm_identify_output.log
pause
endlocal
