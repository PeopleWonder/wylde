@echo off
REM ============================================================
REM Phase 8.1 — Delete the legacy _webcrawler_service/ staging folder.
REM
REM Run this AFTER _phase8_1_webcrawler_smoke.bat has reported PASS.
REM This deletes Extensions\Webcrawler\_webcrawler_service\ and all
REM the Flask-shell files inside it (run.py, startup.py, ipc.py,
REM discovery.py, consul_client.py, manifest.py, errors.py,
REM tool_interface.py, webcrawler_api.py, scraper.py, config.py,
REM tools/{scrape,fetch,extract,__init__}.py, README.md, etc.).
REM
REM extractor.py was already hoisted to Extensions\Webcrawler\.
REM scraper.py and config.py are NOT hoisted — they had no live
REM importers post-refactor (handler.py replaced them inline).
REM ============================================================

setlocal
cd /d "%~dp0"

set "TARGET=Extensions\Webcrawler\_webcrawler_service"

if not exist "%TARGET%" (
    echo No staging folder at "%TARGET%" — already clean.
    echo.
    pause
    endlocal
    exit /b 0
)

echo Deleting staging folder: %TARGET%
echo.
rmdir /S /Q "%TARGET%"
set EXIT_CODE=%ERRORLEVEL%

if %EXIT_CODE% EQU 0 (
    echo OK — _webcrawler_service\ removed.
    echo.
    echo Next step: re-run _phase8_1_webcrawler_smoke.bat to confirm
    echo handler.py still loads cleanly without the staging folder.
) else (
    echo FAIL — rmdir exited %EXIT_CODE%. The folder may be in use
    echo or you may need elevated permissions. Close any Python
    echo processes / file handles on that tree and retry.
)
echo.
pause
endlocal
exit /b %EXIT_CODE%
