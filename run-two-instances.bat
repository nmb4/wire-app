@echo off
setlocal

rem Build and launch a three-participant local Wire group call.
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

echo Starting three-instance full-mesh call...
start "" "%EXE%" --dev-pair

echo Done. Check logs in %LOCALAPPDATA%\wire\

endlocal
