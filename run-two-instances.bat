@echo off
setlocal

rem Build and launch two local Wire instances for peer testing.
rem Logs: %LOCALAPPDATA%\wire\wire-app-<pid>.log

cd /d "%~dp0"

set "RUST_LOG=info,wire=debug,wire_app=debug"

echo Building wire-app (release, --no-default-features)...
cargo build --release -p wire-app --no-default-features
if errorlevel 1 (
    echo Build failed.
    exit /b 1
)

set "EXE=%~dp0target\release\wire-app.exe"
if not exist "%EXE%" (
    echo Executable not found: %EXE%
    exit /b 1
)

echo Starting two instances...
start "" "%EXE%"
start "" "%EXE%"

echo Done. Check logs in %LOCALAPPDATA%\wire\

endlocal