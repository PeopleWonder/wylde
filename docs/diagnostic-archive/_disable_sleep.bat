@echo off
REM ============================================================
REM  Disable sleep + hibernate (the Wylde user's KP41 / crash diagnostic).
REM  Self-elevates via UAC if not already running as admin.
REM ============================================================

net session >nul 2>&1 || (
    echo Need admin. Re-launching elevated...
    powershell -Command "Start-Process '%~f0' -Verb RunAs"
    exit /b
)

REM Anchor working dir to this script's folder (elevation resets cwd to System32).
cd /d "%~dp0"
set "LOG=_disable_sleep_output.log"

echo === disable_sleep run at %DATE% %TIME% === > "%LOG%"
echo. >> "%LOG%"

echo --- powercfg /change standby-timeout-ac 0 --- >> "%LOG%"
powercfg /change standby-timeout-ac 0 >> "%LOG%" 2>&1

echo --- powercfg /change standby-timeout-dc 0 --- >> "%LOG%"
powercfg /change standby-timeout-dc 0 >> "%LOG%" 2>&1

echo --- powercfg /change hibernate-timeout-ac 0 --- >> "%LOG%"
powercfg /change hibernate-timeout-ac 0 >> "%LOG%" 2>&1

echo --- powercfg /change hibernate-timeout-dc 0 --- >> "%LOG%"
powercfg /change hibernate-timeout-dc 0 >> "%LOG%" 2>&1

echo --- powercfg /h off --- >> "%LOG%"
powercfg /h off >> "%LOG%" 2>&1

echo --- powercfg /q SCHEME_CURRENT SUB_SLEEP STANDBYIDLE --- >> "%LOG%"
powercfg /q SCHEME_CURRENT SUB_SLEEP STANDBYIDLE >> "%LOG%" 2>&1

echo. >> "%LOG%"
echo === done at %DATE% %TIME% === >> "%LOG%"

REM Display log on screen (tee-equivalent for the user).
type "%LOG%"
echo.
echo Log written to: %~dp0%LOG%
pause
