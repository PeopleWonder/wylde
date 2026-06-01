@echo off
REM Phase 8.4 — VoiceAssistant slim-down static-import check, vault-root wrapper.
REM Mirrors stdout+stderr to _phase8_4_voice_assistant_check_output.log so the
REM .bat-via-File-Explorer pattern produces a readable artifact.

setlocal
cd /d "%~dp0"

set "PY_SCRIPT=_phase8_4_voice_assistant_check.py"
set "LOG=_phase8_4_voice_assistant_check_output.log"

echo Running Phase 8.4 VoiceAssistant slim-down check...
echo Output will be saved to: %LOG%
echo.

REM Prefer 'py -3' (Python launcher), fall back to 'python' on PATH.
where py >nul 2>nul
if %ERRORLEVEL% EQU 0 (
    set "PY=py -3"
) else (
    set "PY=python"
)

%PY% "%PY_SCRIPT%" > "%LOG%" 2>&1
set EXIT_CODE=%ERRORLEVEL%

type "%LOG%"
echo.
echo ============================================================
if %EXIT_CODE% EQU 0 (
    echo PASS — Phase 8.4 VoiceAssistant slim-down check exited 0.
) else (
    echo FAIL — Phase 8.4 VoiceAssistant slim-down check exited %EXIT_CODE%.
)
echo ============================================================
echo.
pause
endlocal
exit /b %EXIT_CODE%
