@echo off
REM Wrapper for the canonical pytest smoke. The user's
REM _phase8_3_followup_smoke.bat uses bare "python" which on this machine
REM resolves to Python 3.11 (no pytest). We use "py -3" instead — that
REM resolves to 3.14 where pytest is already installed.

setlocal
cd /d "%~dp0"

set "TEST_PATH=Core\harness\tooling\tests\test_smoke.py"
set "LOG=_phase8_3_followup_smoke_output.log"

if not exist "%TEST_PATH%" (
    echo ERROR: cannot find "%TEST_PATH%" > "%LOG%"
    type "%LOG%"
    pause
    exit /b 1
)

py -3 -m pytest -q "%TEST_PATH%" > "%LOG%" 2>&1
set EC=%ERRORLEVEL%

type "%LOG%"
echo.
echo ============================================================
if %EC% EQU 0 (
    echo PASS — pytest smoke exited 0.
) else (
    echo FAIL — pytest smoke exited %EC%.
)
echo ============================================================
echo.
pause
endlocal
exit /b %EC%
