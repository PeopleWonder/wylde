@echo off
cd /d "%~dp0"
powershell.exe -ExecutionPolicy Bypass -File "%~dp0_crash_scan.ps1" > "%~dp0_crash_scan_output.log" 2>&1
