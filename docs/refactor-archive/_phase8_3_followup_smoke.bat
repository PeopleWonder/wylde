@echo off
REM ============================================================
REM Phase 8.3 follow-up smoke runner.
REM
REM Double-click from File Explorer. Runs the canonical pytest
REM smoke test at Core\harness\tooling\tests\test_smoke.py and
REM mirrors the output to _phase8_3_followup_smoke_output.log
REM next to this file so the assistant can read it back.
REM
REM This wrapper exists only because the assistant cannot launch
REM Windows .bat files itself. Day-to-day, just run
REM Core\harness\tooling\tests\run_smoke_test.bat directly.
REM ============================================================

setlocal
cd /d "%~dp0"

set "LOG=_phase8_3_followup_smoke_output.log"
set "TEST_PATH=Core\harness\tooling\tests\test_smoke.py"

if not exist "%TEST_PATH%" (
    echo ERROR: cannot find "%TEST_PATH%" > "%LOG%"
    type "%LOG%"
    pause
    endlocal
    exit /b 1
)

py -3 -m pytest -q "%TEST_PATH%" > "%LOG%" 2>&1
set EXIT_CODE=%ERRORLEVEL%

type "%LOG%"
echo.
echo ============================================================
if %EXIT_CODE% EQU 0 (
    echo PASS — pytest smoke exited 0.
) else (
    echo FAIL — pytest smoke exited %EXIT_CODE%.
)
echo ============================================================
echo.
pause
endlocal
exit /b %EXIT_CODE%
