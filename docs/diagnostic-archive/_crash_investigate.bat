@echo off
cd /d "%~dp0"
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0_crash_investigate.ps1"
echo.
echo Done. Output written to _crash_investigate_output.log
timeout /t 3 >nul
