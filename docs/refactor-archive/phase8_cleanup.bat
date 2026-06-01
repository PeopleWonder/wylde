@echo off
REM ============================================================
REM Phase 8 cleanup — vault-root migration artifacts + superseded
REM staged-legacy folders. Double-click from File Explorer.
REM
REM the Wylde user is keeping Wylde/_legacy/ and Wylde/default/ — this script
REM does NOT touch them. the Wylde user is also keeping the per-service
REM "_legacy" folders inside Lifecycle, N8N, and harness/orchestrator_api.
REM
REM What this deletes:
REM   Task A — vault-root operational scripts/logs/flag from earlier phases
REM   Task B — Phase 5A staged Flask code that Phase 6 superseded:
REM            Core/harness/tooling/tool_registry/_tool_registry_legacy/
REM            Core/harness/tooling/tool_runner/_tool_runner_legacy/
REM
REM Replacements verified present before this script was written:
REM   Core/harness/tooling/tool_registry/__init__.py  (catalog API)
REM   Core/harness/tooling/tool_runner/__init__.py    (run_tool dispatcher)
REM
REM Pre-audited present targets only — files/folders that were already
REM cleaned up in earlier phases are NOT included (no spurious "missing"
REM warnings).
REM ============================================================

setlocal
cd /d "%~dp0"

echo.
echo Phase 8 cleanup — running in: %CD%
echo.

set DELETED=0
set SKIPPED=0

REM ------------------------------------------------------------
REM Task A — vault-root operational artifacts (15 files, 2 folders)
REM ------------------------------------------------------------
echo [Task A] Deleting vault-root migration artifacts...

call :del_file "phase4a.bat"
call :del_file "phase4a.ps1"
call :del_file "phase4a-output.log"
call :del_file "_run-memgraph-migration.bat"
call :del_file "_run-memgraph-migration.ps1"
call :del_file "migrate_memgraph.bat"
call :del_file "migrate_memgraph.ps1"
call :del_file "memgraph_migration.log"
call :del_file "phase5.bat"
call :del_file "phase5.ps1"
call :del_file "phase5.log"
call :del_file "_smoke_test.bat"
call :del_file "_smoke_test.ps1"
call :del_file "_smoke_test_output.log"
call :del_file "_ext_bridge_smoke.bat"
call :del_file "_ext_bridge_smoke.ps1"

call :del_folder "_phase5c"
call :del_folder "_phase5e"

REM ------------------------------------------------------------
REM Task B — superseded staged-legacy tooling folders
REM ------------------------------------------------------------
echo.
echo [Task B] Deleting Phase 5A tool_registry/tool_runner Flask staging...

call :del_folder "Core\harness\tooling\tool_registry\_tool_registry_legacy"
call :del_folder "Core\harness\tooling\tool_runner\_tool_runner_legacy"

echo.
echo ============================================================
echo Phase 8 cleanup complete.
echo   Deleted: %DELETED% target(s)
echo   Skipped: %SKIPPED% (not present)
echo ============================================================
echo.
echo NEXT STEPS:
echo   1. Verify the report counts above match expectations.
echo   2. After confirming the deletions, you can delete THIS file too:
echo      phase8_cleanup.bat
echo.
pause
endlocal
exit /b 0


REM ------------------------------------------------------------
REM Subroutines
REM ------------------------------------------------------------
:del_file
if exist "%~1" (
    del /f /q "%~1" 2>nul
    if exist "%~1" (
        echo   FAIL  %~1   ^(could not delete^)
    ) else (
        echo   ok    %~1
        set /a DELETED+=1
    )
) else (
    echo   skip  %~1   ^(not present^)
    set /a SKIPPED+=1
)
exit /b 0

:del_folder
if exist "%~1" (
    rmdir /s /q "%~1" 2>nul
    if exist "%~1" (
        echo   FAIL  %~1\   ^(could not delete^)
    ) else (
        echo   ok    %~1\
        set /a DELETED+=1
    )
) else (
    echo   skip  %~1\   ^(not present^)
    set /a SKIPPED+=1
)
exit /b 0
