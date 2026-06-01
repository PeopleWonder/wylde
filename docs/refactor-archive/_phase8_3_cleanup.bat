@echo off
REM ============================================================
REM Phase 8.3 follow-up — punch-list item #3 cleanup.
REM
REM Double-click from File Explorer. Runs three destructive ops
REM the assistant needs the user-side filesystem to perform:
REM
REM   1. Remove the now-empty N8N\templates folder
REM      (contents already moved to N8N\workflow_templates).
REM   2. Remove the vault-root standalone smoke wrappers
REM      (_phase8_3_n8n_smoke.bat / .ps1 / .py / .log).
REM   3. Recursively remove N8N\_n8n_service_merge — every file
REM      worth keeping was hoisted in Phase 8.
REM
REM Output is mirrored to _phase8_3_cleanup_output.log next to
REM this file so the assistant can read it back.
REM ============================================================

setlocal
cd /d "%~dp0"

set "LOG=_phase8_3_cleanup_output.log"

(
    echo === Phase 8.3 cleanup ===
    echo CWD: %CD%
    echo.

    echo [1/3] Remove N8N\templates [must be empty]...
    if exist "N8N\templates" (
        rmdir "N8N\templates"
        if exist "N8N\templates" (
            echo   FAIL — N8N\templates still exists. Contents:
            dir /b "N8N\templates"
        ) else (
            echo   OK — N8N\templates removed.
        )
    ) else (
        echo   SKIP — N8N\templates already gone.
    )
    echo.

    echo [2/3] Remove vault-root standalone smoke wrappers...
    for %%F in (
        _phase8_3_n8n_smoke.bat
        _phase8_3_n8n_smoke.ps1
        _phase8_3_n8n_smoke_check.py
        _phase8_3_n8n_smoke_output.log
    ) do (
        if exist "%%F" (
            del /f /q "%%F"
            if exist "%%F" (
                echo   FAIL — %%F still present.
            ) else (
                echo   OK — deleted %%F
            )
        ) else (
            echo   SKIP — %%F not present.
        )
    )
    echo.

    echo [3/3] Remove N8N\_n8n_service_merge recursively...
    if exist "N8N\_n8n_service_merge" (
        rmdir /s /q "N8N\_n8n_service_merge"
        if exist "N8N\_n8n_service_merge" (
            echo   FAIL — N8N\_n8n_service_merge still exists.
        ) else (
            echo   OK — N8N\_n8n_service_merge removed.
        )
    ) else (
        echo   SKIP — N8N\_n8n_service_merge already gone.
    )
    echo.

    echo === Cleanup done ===
) > "%LOG%" 2>&1

type "%LOG%"
echo.
pause
endlocal
