@echo off
setlocal enabledelayedexpansion

:: wylde-memgraph launcher (native Windows, no Docker)
::
:: This bat handles the venv + Neo4j vendor download. The Python entrypoint
:: (run.py) is responsible for spawning Neo4j as a managed child process so
:: it remains under the wylde-memgraph parent in Task Manager (rather than a
:: detached `start /min` console).
::
:: wylde-rag connects to the named pipe via ipc.send("wylde-memgraph", ...).
:: The Bolt port is an internal implementation detail; nothing talks to it
:: directly from outside this service.
::
:: First-run bootstrap (when %SERVICE_DIR%\vendor\neo4j is missing):
::   download + extract JDK 21 (Microsoft build) to vendor\jdk,
::   download + extract Neo4j Community to vendor\neo4j,
::   patch conf\neo4j.conf with Wylde overrides.

set SERVICE_DIR=%~dp0
if "%SERVICE_DIR:~-1%"=="\" set SERVICE_DIR=%SERVICE_DIR:~0,-1%

set VENDOR_DIR=%SERVICE_DIR%\vendor
set JDK_DIR=%VENDOR_DIR%\jdk
set NEO4J_DIR=%VENDOR_DIR%\neo4j
set DATA_DIR=%SERVICE_DIR%\data
set LOGS_DIR=%SERVICE_DIR%\logs

set JDK_URL=https://aka.ms/download-jdk/microsoft-jdk-21-windows-x64.zip
set NEO4J_URL=https://dist.neo4j.org/neo4j-community-2026.03.1-windows.zip

title wylde-memgraph (Neo4j + pipe service)

echo ============================================================
echo  wylde-memgraph - Bolt knowledge graph (Neo4j Community)
echo ============================================================

:: ----------------------------------------------------------------
:: 1. First-run: fetch JDK if missing
:: ----------------------------------------------------------------
if not exist "%JDK_DIR%\bin\java.exe" (
    echo [SETUP] Downloading Microsoft OpenJDK 21...
    if not exist "%VENDOR_DIR%" mkdir "%VENDOR_DIR%"
    powershell -NoProfile -ExecutionPolicy Bypass -Command ^
      "Invoke-WebRequest -Uri '%JDK_URL%' -OutFile '%VENDOR_DIR%\jdk.zip' -UseBasicParsing"
    if errorlevel 1 (
        echo [ERROR] JDK download failed.
        exit /b 1
    )
    echo [SETUP] Extracting JDK...
    powershell -NoProfile -ExecutionPolicy Bypass -Command ^
      "Expand-Archive -Path '%VENDOR_DIR%\jdk.zip' -DestinationPath '%VENDOR_DIR%' -Force"
    :: JDK ZIP extracts to jdk-21.x.y+z\ ; rename to jdk\
    for /d %%D in ("%VENDOR_DIR%\jdk-21*") do move /y "%%D" "%JDK_DIR%" >nul
    del /q "%VENDOR_DIR%\jdk.zip"
    if not exist "%JDK_DIR%\bin\java.exe" (
        echo [ERROR] JDK extraction failed.
        exit /b 1
    )
    echo [SETUP] JDK OK at %JDK_DIR%
)

:: ----------------------------------------------------------------
:: 2. First-run: fetch Neo4j if missing
:: ----------------------------------------------------------------
if not exist "%NEO4J_DIR%\bin\neo4j.bat" (
    echo [SETUP] Downloading Neo4j Community Edition...
    powershell -NoProfile -ExecutionPolicy Bypass -Command ^
      "Invoke-WebRequest -Uri '%NEO4J_URL%' -OutFile '%VENDOR_DIR%\neo4j.zip' -UseBasicParsing"
    if errorlevel 1 (
        echo [ERROR] Neo4j download failed.
        exit /b 1
    )
    echo [SETUP] Extracting Neo4j...
    powershell -NoProfile -ExecutionPolicy Bypass -Command ^
      "Expand-Archive -Path '%VENDOR_DIR%\neo4j.zip' -DestinationPath '%VENDOR_DIR%' -Force"
    for /d %%D in ("%VENDOR_DIR%\neo4j-community-*") do move /y "%%D" "%NEO4J_DIR%" >nul
    del /q "%VENDOR_DIR%\neo4j.zip"
    if not exist "%NEO4J_DIR%\bin\neo4j.bat" (
        echo [ERROR] Neo4j extraction failed.
        exit /b 1
    )
    :: Patch conf\neo4j.conf with Wylde overrides (only on fresh extract)
    call :patch_conf
    echo [SETUP] Neo4j OK at %NEO4J_DIR%
)

:: ----------------------------------------------------------------
:: 3. Ensure patch is applied (idempotent)
:: ----------------------------------------------------------------
findstr /c:"# --- WYLDE OVERRIDES ---" "%NEO4J_DIR%\conf\neo4j.conf" >nul 2>&1
if errorlevel 1 (
    echo [SETUP] Applying Wylde conf overrides...
    call :patch_conf
)

:: ----------------------------------------------------------------
:: 4. Data / logs dirs
:: ----------------------------------------------------------------
if not exist "%DATA_DIR%" mkdir "%DATA_DIR%"
if not exist "%LOGS_DIR%" mkdir "%LOGS_DIR%"

:: ----------------------------------------------------------------
:: 5. Environment for neo4j.bat
:: ----------------------------------------------------------------
set JAVA_HOME=%JDK_DIR%
set NEO4J_HOME=%NEO4J_DIR%
set PATH=%JDK_DIR%\bin;%PATH%

:: conf\neo4j.conf pins server.directories.data / .logs / .transaction.logs.root
:: at the Wylde-owned directories (outside vendor\neo4j) so persistent data
:: survives a re-extract of the vendor directory.
set NEO4J_CONF=%NEO4J_DIR%\conf

echo [INFO]  Neo4j config:
echo         JAVA_HOME:   %JAVA_HOME%
echo         NEO4J_HOME:  %NEO4J_HOME%
echo         Data dir:    %DATA_DIR%
echo         Bolt:        bolt://127.0.0.1:7687  (auth disabled)
echo.

:: Neo4j is launched by run.py as a managed subprocess (CREATE_NO_WINDOW,
:: stdout piped to logs/neo4j.log) so it remains a child of the wylde-memgraph
:: pythonw process and dies when run.py exits.

:: ----------------------------------------------------------------
:: 6. Python venv for the Wylde named-pipe service
:: ----------------------------------------------------------------
set VENV_DIR=%SERVICE_DIR%\venv
set VENV_PYTHON=%VENV_DIR%\Scripts\pythonw.exe
set VENV_PIP=%VENV_DIR%\Scripts\pip.exe

if not exist "%VENV_PYTHON%" (
    echo [SETUP] Creating Python venv for wylde-memgraph pipe service...
    python -m venv "%VENV_DIR%"
    if errorlevel 1 (
        echo [ERROR] Failed to create venv. Is Python 3.10+ installed?
        exit /b 1
    )
)

echo [SETUP] Installing pipe-service dependencies...
"%VENV_PIP%" install --upgrade pip --quiet
"%VENV_PIP%" install -r "%SERVICE_DIR%\requirements.txt" --quiet
if errorlevel 1 (
    echo [ERROR] Dependency installation failed.
    exit /b 1
)
echo [SETUP] Dependencies OK

:: ----------------------------------------------------------------
:: 7. Start the Wylde named-pipe service (run.py spawns Neo4j as a
::    managed child, then serves on \\.\pipe\wylde-memgraph)
:: ----------------------------------------------------------------
echo [START] Starting wylde-memgraph pipe service on \\.\pipe\wylde-memgraph ...
echo.

cd /d "%SERVICE_DIR%"
"%VENV_PYTHON%" run.py
set RC=%ERRORLEVEL%

echo.
echo [EXIT] wylde-memgraph stopped (rc=%RC%).
exit /b %RC%


:: ================================================================
:: patch_conf — append Wylde overrides to %NEO4J_DIR%\conf\neo4j.conf.
:: Idempotent: caller checks the WYLDE OVERRIDES marker before invoking.
:: ================================================================
:patch_conf
:: Rewrite conf so the file contains exactly ONE Wylde block at the end:
::   1. Drop every line at/after any existing "# --- WYLDE OVERRIDES ---"
::      marker (makes patch_conf fully idempotent even if it runs twice).
::   2. Comment out any uncommented shipped keys that the override block
::      re-declares. Neo4j 2026.x refuses to start if a key appears twice,
::      even with identical values.
:: Writes UTF-8 (no BOM) so findstr can read the marker on the next boot;
:: Set-Content's default (UTF-16 LE w/ BOM on PS 5.x) breaks findstr.
powershell -NoProfile -ExecutionPolicy Bypass -Command ^
  "$p = '%NEO4J_DIR%\conf\neo4j.conf';" ^
  "$keys = @('server.bolt.enabled','server.http.enabled','server.https.enabled','server.memory.heap.initial_size','server.memory.heap.max_size','server.memory.pagecache.size','server.default_listen_address','server.bolt.tls_level','server.bolt.listen_address','server.bolt.advertised_address','server.directories.data','server.directories.logs','server.directories.transaction.logs.root','dbms.security.auth_enabled','dbms.usage_report.enabled');" ^
  "$lines = Get-Content -Path $p -Encoding UTF8;" ^
  "$cut = @(); foreach ($L in $lines) { if ($L -match '^# --- WYLDE OVERRIDES') { break } ; $cut += $L };" ^
  "while ($cut.Count -gt 0 -and [string]::IsNullOrWhiteSpace($cut[-1])) { $cut = $cut[0..($cut.Count - 2)] };" ^
  "$out = foreach ($L in $cut) { $m=$false; foreach ($k in $keys) { if ($L -match ('^' + [regex]::Escape($k) + '\s*=')) { $m=$true; break } }; if ($m) { '# ' + $L } else { $L } };" ^
  "[IO.File]::WriteAllLines($p, $out, (New-Object System.Text.UTF8Encoding $false))"

:: Compute absolute, forward-slash paths for the data/logs dirs. Neo4j 2026.x
:: rejects unnormalized relative paths (../../...) so we hand it absolute
:: paths instead. Forward slashes work on Windows and avoid the conf-file
:: backslash-escape rules.
set "DATA_DIR_FWD=%DATA_DIR:\=/%"
set "LOGS_DIR_FWD=%LOGS_DIR:\=/%"
(
  echo.
  echo # --- WYLDE OVERRIDES ---
  echo # Appended by core\wylde-memgraph\start_graph.bat on first run.
  echo # Persistent graph data lives outside vendor\neo4j so the vendor
  echo # directory can be re-extracted without losing state.
  echo dbms.security.auth_enabled=false
  echo dbms.usage_report.enabled=false
  echo server.default_listen_address=127.0.0.1
  echo server.bolt.enabled=true
  echo server.bolt.tls_level=DISABLED
  echo server.bolt.listen_address=127.0.0.1:7687
  echo server.bolt.advertised_address=127.0.0.1:7687
  echo server.http.enabled=false
  echo server.memory.heap.initial_size=512m
  echo server.memory.heap.max_size=1g
  echo server.memory.pagecache.size=512m
  echo server.directories.data=%DATA_DIR_FWD%
  echo server.directories.logs=%LOGS_DIR_FWD%
  echo server.directories.transaction.logs.root=%DATA_DIR_FWD%/transactions
) >> "%NEO4J_DIR%\conf\neo4j.conf"
exit /b 0
