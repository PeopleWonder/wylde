@echo off
REM Starts the Wylde build watcher. Leave this window open while working
REM with an agent session; Ctrl+C (or closing the window) stops it.
REM No admin / UAC required.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0build-watcher.ps1"
pause
