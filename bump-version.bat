@echo off
powershell -NoProfile -ExecutionPolicy Bypass -File "%~dp0bump-version.ps1" %*
exit /b %errorlevel%
