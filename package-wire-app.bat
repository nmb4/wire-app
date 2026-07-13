@echo off
setlocal EnableExtensions DisableDelayedExpansion

for /f "usebackq delims=" %%V in (`powershell -NoProfile -Command "$metadata = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json; ($metadata.packages | Where-Object { $_.name -eq 'wire-app' }).version"`) do set "VERSION=%%V"
if not defined VERSION (
    echo Failed to read the wire-app version from Cargo.toml.
    exit /b 1
)

set "EXE_NAME=wire-app.exe"
set "ZIP_NAME=wire-app-v%VERSION%.zip"
set "ARTIFACT_DIR=dist"
set "EXE_PATH=%ARTIFACT_DIR%\%EXE_NAME%"
set "ZIP_PATH=%ARTIFACT_DIR%\%ZIP_NAME%"

if not exist "%ARTIFACT_DIR%" mkdir "%ARTIFACT_DIR%"
if errorlevel 1 (
    echo Failed to create the artifact directory: %ARTIFACT_DIR%
    exit /b 1
)

echo Building release build...
call :cargo_quiet build --release -p wire-app --no-default-features
if errorlevel 1 (
    echo Build failed.
    exit /b 1
)

echo Creating %EXE_NAME%...
copy /Y "target\release\wire-app.exe" "%EXE_PATH%" >nul
if errorlevel 1 (
    echo Failed to copy the release executable.
    exit /b 1
)

echo Packaging %ZIP_NAME%...
powershell -NoProfile -Command "$ErrorActionPreference = 'Stop'; for ($attempt = 1; $attempt -le 5; $attempt++) { try { Compress-Archive -LiteralPath '%EXE_PATH%' -DestinationPath '%ZIP_PATH%' -Force; exit 0 } catch { if ($attempt -eq 5) { throw }; Start-Sleep -Milliseconds 500 } }"
if errorlevel 1 (
    echo Failed to create the release archive.
    exit /b 1
)

echo Done. Created %EXE_PATH% and %ZIP_PATH%.
endlocal
exit /b 0


:cargo_quiet
setlocal DisableDelayedExpansion
set "CARGO_LOG=%TEMP%\cargo-%RANDOM%-%RANDOM%.log"

cargo %* > "%CARGO_LOG%" 2>&1
set "CARGO_EXIT=%ERRORLEVEL%"

if not "%CARGO_EXIT%"=="0" (
    type "%CARGO_LOG%" >&2
)

del "%CARGO_LOG%" >nul 2>&1
endlocal & exit /b %CARGO_EXIT%
