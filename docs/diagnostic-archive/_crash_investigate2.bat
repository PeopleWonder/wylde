@echo off
cd /d "%~dp0"
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0_crash_investigate2.ps1"
echo.
echo Done. Output written to _crash_investigate2_output.log
timeout /t 3 >nul
