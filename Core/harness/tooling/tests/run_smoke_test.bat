@echo off
REM Phase 8 — harness/tooling smoke test (n8n service merge).
REM
REM Double-click from File Explorer to run the catalog + runner +
REM confirmation-gate checks. CWD becomes the location of this .bat;
REM we resolve everything relative to that.

setlocal

set "TEST_DIR=%~dp0"
set "TEST_PATH=%TEST_DIR%test_smoke.py"

echo Running harness/tooling smoke test: "%TEST_PATH%"
echo.

REM Use pytest if available (the existing tests are pytest-style); fall
REM back to python directly so the file at least imports cleanly.
python -m pytest -q "%TEST_PATH%"
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
