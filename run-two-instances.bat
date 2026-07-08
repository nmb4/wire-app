@echo off
setlocal

rem Build and launch two local Callme instances for peer testing.
rem Logs: %LOCALAPPDATA%\callme\callme-<pid>.log

cd /d "%~dp0"

set "RUST_LOG=info,callme=debug,callme_egui=debug"

echo Building callme-egui (release, --no-default-features)...
cargo build --release -p callme-egui --no-default-features
if errorlevel 1 (
    echo Build failed.
    exit /b 1
)

set "EXE=%~dp0target\release\callme-egui.exe"
if not exist "%EXE%" (
    echo Executable not found: %EXE%
    exit /b 1
)

echo Starting two instances...
start "" "%EXE%"
start "" "%EXE%"

echo Done. Check logs in %LOCALAPPDATA%\callme\

endlocal