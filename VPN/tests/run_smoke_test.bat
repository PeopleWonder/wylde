@echo off
REM Phase 9 — Wylde VPN smoke test.
REM
REM Static-import + structural sanity check. Does NOT exercise wg-quick,
REM iptables, named-pipe IPC, or any peer registration flow. For real
REM behavioural checks run the integration harness on a Linux host
REM with WireGuard installed.

setlocal

set "TEST_DIR=%~dp0"
set "TEST_PATH=%TEST_DIR%test_smoke.py"

echo Running Wylde VPN smoke test: "%TEST_PATH%"
echo.

py -3 -m pytest -q "%TEST_PATH%"
set EXIT_CODE=%ERRORLEVEL%

echo.
if %EXIT_CODE% EQU 0 (
    echo OK -- Wylde VPN smoke checks passed.
) else (
    echo FAIL -- exit code %EXIT_CODE%. Scroll up for details.
)
echo.
pause

endlocal
exit /b %EXIT_CODE%
