@echo off
REM Wrapper that invokes _phase8_3_cleanup.bat via cmd /c so the launch path
REM goes through cmd directly rather than File-Explorer shell-execute.

setlocal
cd /d "%~dp0"

set "INNER=_phase8_3_cleanup.bat"
set "WRAP_LOG=_run_phase8_3_cleanup_output.log"

if not exist "%INNER%" (
    echo ERROR: %INNER% not found > "%WRAP_LOG%"
    type "%WRAP_LOG%"
    pause
    exit /b 1
)

REM The original _phase8_3_cleanup.bat has a cmd-parser bug (paren inside
REM a parenthesised echo block) — exits 255 with "... was unexpected at
REM this time" before any work runs. Re-implement the same target list
REM in PowerShell instead.
powershell.exe -NoProfile -ExecutionPolicy Bypass -File "%~dp0_run_phase8_3_cleanup.ps1"
set EC=%ERRORLEVEL%

if exist "_phase8_3_cleanup_output.log" (
    type "_phase8_3_cleanup_output.log" > "%WRAP_LOG%"
) else (
    echo no _phase8_3_cleanup_output.log produced > "%WRAP_LOG%"
)

type "%WRAP_LOG%"
echo.
echo ============================================================
echo Inner exit code: %EC%
echo ============================================================
echo.
pause
endlocal
exit /b %EC%
