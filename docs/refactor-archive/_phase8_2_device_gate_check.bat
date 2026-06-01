@echo off
REM Phase 8.2 — Device Gate static-import check, vault-root wrapper.
REM Mirrors stdout+stderr to _phase8_2_device_gate_check_output.log so the
REM .bat-via-File-Explorer pattern produces a readable artifact.

setlocal
cd /d "%~dp0"

set "PY_SCRIPT=_phase8_2_device_gate_check.py"
set "LOG=_phase8_2_device_gate_check_output.log"

echo Running Phase 8.2 Device Gate check...
echo Output will be saved to: %LOG%
echo.

REM Prefer 'py -3' (Python launcher), fall back to 'python' on PATH.
where py >nul 2>nul
if %ERRORLEVEL% EQU 0 (
    set "PY=py -3"
) else (
    set "PY=python"
)

REM Ensure flask is available so the check produces a full 'ok:' result
REM rather than a soft-pass. flask_cors is also imported by device_gate.
%PY% -c "import flask, flask_cors" >nul 2>&1
if %ERRORLEVEL% NEQ 0 (
    echo flask / flask_cors missing — installing to user site... >> "%LOG%"
    %PY% -m pip install --user flask flask-cors >> "%LOG%" 2>&1
)

%PY% "%PY_SCRIPT%" >> "%LOG%" 2>&1
set EXIT_CODE=%ERRORLEVEL%

type "%LOG%"
echo.
echo ============================================================
if %EXIT_CODE% EQU 0 (
    echo PASS — Phase 8.2 device gate check exited 0.
) else (
    echo FAIL — Phase 8.2 device gate check exited %EXIT_CODE%.
)
echo ============================================================
echo.
pause
endlocal
exit /b %EXIT_CODE%
