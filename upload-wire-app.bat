@echo off
setlocal

for /f "usebackq delims=" %%V in (`powershell -NoProfile -Command "$metadata = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json; ($metadata.packages | Where-Object { $_.name -eq 'wire-app' }).version"`) do set "VERSION=%%V"
if not defined VERSION (
    echo Failed to read the wire-app version from Cargo.toml.
    exit /b 1
)

set "ZIP_NAME=wire-app-v%VERSION%.zip"
set "ZIP_PATH=dist\%ZIP_NAME%"
set "UPLOAD_URL=https://api.stardive.space/v1/files"

if not exist "%ZIP_PATH%" (
    echo Release archive not found: %ZIP_PATH%
    echo Run package-wire-app.bat first.
    exit /b 1
)

if /I "%~1"=="--dry-run" (
    echo Would upload %ZIP_PATH% to %UPLOAD_URL%.
    exit /b 0
)

set "RESPONSE_FILE=%TEMP%\wire-upload-%RANDOM%-%RANDOM%.json"
echo Uploading %ZIP_PATH%...
curl.exe --silent --show-error --fail-with-body -F "file=@%ZIP_PATH%;type=application/zip" -o "%RESPONSE_FILE%" "%UPLOAD_URL%"
if errorlevel 1 (
    echo Upload failed.
    if exist "%RESPONSE_FILE%" type "%RESPONSE_FILE%"
    if exist "%RESPONSE_FILE%" del /Q "%RESPONSE_FILE%"
    exit /b 1
)

set "WIRE_UPLOAD_RESPONSE=%RESPONSE_FILE%"
for /f "usebackq delims=" %%I in (`powershell -NoProfile -Command "$response = Get-Content -Raw $env:WIRE_UPLOAD_RESPONSE | ConvertFrom-Json; if ($response.id) { $response.id } elseif ($response.file.id) { $response.file.id } else { exit 1 }"`) do set "FILE_ID=%%I"

if not defined FILE_ID (
    echo Upload completed, but the API response did not contain a file ID:
    type "%RESPONSE_FILE%"
    del /Q "%RESPONSE_FILE%"
    exit /b 1
)

del /Q "%RESPONSE_FILE%"
echo Upload complete.
echo File ID: %FILE_ID%
echo Download: https://api.stardive.space/v1/files/%FILE_ID%
endlocal