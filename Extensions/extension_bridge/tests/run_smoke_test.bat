@echo off
REM Phase 7 — Extension Bridge smoke test.
REM
REM Double-click this from File Explorer (.bat-via-File-Explorer pattern)
REM to run the smoke checks. The script's CWD becomes the location of
REM this .bat — we resolve the test path relative to that.

setlocal

set "TEST_PATH=%~dp0smoke_test.py"
echo Running Extension Bridge smoke test: "%TEST_PATH%"
echo.

REM Use the system "python" launcher; if the Wylde user prefers a venv, swap
REM the line below for the venv path.
python "%TEST_PATH%"
set EXIT_CODE=%ERRORLEVEL%

echo.
if %EXIT_CODE% EQU 0 (
    echo OK — all smoke checks passed.
) else (
    echo FAIL — exit code %EXIT_CODE%. Scroll up for details.
)
echo.
pause

endlocal
exit /b %EXIT_CODE%
