@echo off
rem Runs the Windows check next to this file. It sends nothing anywhere and changes no
rem setting; see the top of test-on-windows.ps1 for exactly what it does.
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0test-on-windows.ps1" %*
echo.
pause
