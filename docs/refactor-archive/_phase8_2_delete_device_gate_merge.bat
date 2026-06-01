@echo off
REM Phase 8.2 — delete Device Gate\_device_gate_merge staging folder.
REM
REM Run this by double-clicking it from File Explorer (direct shell access
REM from the agent is unreliable). It removes the entire staging folder.
REM Safe to run twice; the rd command no-ops if the folder is gone.
REM
REM After this completes, the only thing left under "Device Gate\" should be:
REM   device_gate.py
REM   data\htpasswd
REM   data\NOTE.md

setlocal
set "TARGET=%~dp0Device Gate\_device_gate_merge"

if not exist "%TARGET%" (
    echo Already gone: %TARGET%
    goto :done
)

echo Deleting: %TARGET%
rd /s /q "%TARGET%"

if exist "%TARGET%" (
    echo FAILED — folder still present. May be locked by an open editor or shell.
    pause
    exit /b 1
)

echo Done.

:done
echo.
echo This .bat is itself part of Phase 8.2 cleanup. You can delete it now.
pause
