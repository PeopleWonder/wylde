@echo off
REM Phase 5+ — model_registry smoke test (unified registry, kind taxonomy).
REM
REM Double-click from File Explorer to run the heuristic + scanner +
REM manifest-override + list_models filter checks. CWD becomes the location
REM of this .bat; we resolve everything relative to that.

setlocal

set "TEST_DIR=%~dp0"
set "TEST_PATH=%TEST_DIR%test_model_registry.py"

echo Running model_registry smoke test: "%TEST_PATH%"
echo.

REM Use pytest (the existing tests are pytest-style).
python -m pytest -q "%TEST_PATH%"
set EXIT_CODE=%ERRORLEVEL%

echo.
if %EXIT_CODE% EQU 0 (
    echo OK -- all model_registry smoke checks passed.
) else (
    echo FAIL -- exit code %EXIT_CODE%. Scroll up for details.
)
echo.
pause

endlocal
exit /b %EXIT_CODE%
