@echo off
REM ============================================================
REM Phase 8.1 Webcrawler smoke test — vault-root wrapper.
REM
REM Double-click from File Explorer. This wrapper calls the real
REM smoke-test bat at:
REM   Wylde/Extensions/Webcrawler/tests/run_smoke_test.bat
REM
REM Output is mirrored to _phase8_1_webcrawler_smoke_output.log
REM next to this file so you can paste the log back to the assistant.
REM ============================================================

setlocal
cd /d "%~dp0"

set "SMOKE_BAT=Extensions\Webcrawler\tests\run_smoke_test.bat"
set "LOG=_phase8_1_webcrawler_smoke_output.log"

if not exist "%SMOKE_BAT%" (
    echo ERROR: cannot find "%SMOKE_BAT%"
    echo The Phase 8.1 smoke test bat is missing or has moved.
    echo.
    pause
    endlocal
    exit /b 1
)

echo Running Phase 8.1 Webcrawler smoke test...
echo Output will be saved to: %LOG%
echo.

REM Strip the inner pause so this wrapper is non-interactive — the
REM wrapper's own pause at the end is enough.
REM Use cmd /c so the inner script's exit code propagates.
cmd /c "%SMOKE_BAT% < nul" > "%LOG%" 2>&1
set EXIT_CODE=%ERRORLEVEL%

type "%LOG%"
echo.
echo ============================================================
if %EXIT_CODE% EQU 0 (
    echo PASS — Phase 8.1 smoke test exited 0.
) else (
    echo FAIL — Phase 8.1 smoke test exited %EXIT_CODE%.
    echo Full output saved to: %LOG%
)
echo ============================================================
echo.
pause
endlocal
exit /b %EXIT_CODE%
