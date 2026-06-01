@echo off
echo ============================================================
echo   Windows Memory Diagnostic - schedule for next reboot
echo ============================================================
echo.
echo the Wylde user - when you're ready, REBOOT the machine (save your work first).
echo The test runs automatically on boot.
echo Takes 15-60 minutes depending on RAM size.
echo.
echo Results show in Event Viewer under:
echo   Windows Logs ^> System
echo   Source = MemoryDiagnostics-Results
echo.
echo Press a key to launch the scheduling dialog (mdsched.exe)...
pause >nul

mdsched.exe

echo.
echo Dialog launched. Pick "Restart now" or "Check on next restart".
pause
