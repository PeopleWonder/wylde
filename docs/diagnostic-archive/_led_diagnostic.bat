@echo off
setlocal
cd /d "%~dp0"
echo Running LED / RGB crash diagnostic...
echo Output will be written to _led_diagnostic_output.log
echo.
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0_led_diagnostic.ps1"
echo.
echo Done. Log saved to %~dp0_led_diagnostic_output.log
echo.
pause
