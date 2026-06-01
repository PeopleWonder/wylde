@echo off
setlocal enabledelayedexpansion

:: wylde-vpn launcher (native Windows, no Docker)
::
:: NOTE: only the management API runs on Windows. Tunnel control endpoints
:: depend on wg-quick/iptables/boringtun which are Linux-only.

set SERVICE_DIR=%~dp0
set VENV_DIR=%SERVICE_DIR%venv
set PYTHON=python

title wylde-vpn (port 8020)

echo ============================================================
echo  wylde-vpn - management API (tunnel control = Linux only)
echo ============================================================

if not exist "%VENV_DIR%\Scripts\python.exe" (
    echo [SETUP] Creating virtual environment...
    %PYTHON% -m venv "%VENV_DIR%"
    if errorlevel 1 (
        echo [ERROR] Failed to create venv. Is Python 3.10+ installed?
        exit /b 1
    )
    echo [SETUP] Venv created at %VENV_DIR%
)

set VENV_PYTHON=%VENV_DIR%\Scripts\pythonw.exe
set VENV_PIP=%VENV_DIR%\Scripts\pip.exe

"%VENV_PIP%" install --upgrade pip --quiet
echo [SETUP] Installing service dependencies...
"%VENV_PIP%" install -r "%SERVICE_DIR%requirements.txt" --quiet
if errorlevel 1 (
    echo [ERROR] Dependency installation failed.
    exit /b 1
)
echo [SETUP] Dependencies OK

set CONSUL_HTTP_ADDR=http://127.0.0.1:8500

echo [START] Launching wylde-vpn on port 8020...
echo.

cd /d "%SERVICE_DIR%"
"%VENV_PYTHON%" run.py
set RC=%ERRORLEVEL%

echo.
echo [EXIT] wylde-vpn stopped (rc=%RC%).
exit /b %RC%
