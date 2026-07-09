@echo off
setlocal

set "PART=%~1"
if not defined PART set "PART=patch"
set "UPLOAD_ARG=%~2"

if /I not "%PART%"=="patch" if /I not "%PART%"=="minor" (
    echo Usage: release-egui.bat [patch^|minor] [--dry-run]
    exit /b 1
)
if defined UPLOAD_ARG if /I not "%UPLOAD_ARG%"=="--dry-run" (
    echo Usage: release-egui.bat [patch^|minor] [--dry-run]
    exit /b 1
)

echo Step 1/3: bumping the %PART% version...
call "%~dp0bump-version.bat" "%PART%"
if errorlevel 1 (
    echo Release stopped during version bump.
    exit /b 1
)

echo.
echo Step 2/3: building and packaging...
call "%~dp0package-egui.bat"
if errorlevel 1 (
    echo Release stopped during packaging.
    exit /b 1
)

echo.
echo Step 3/3: uploading...
call "%~dp0upload-egui.bat" %UPLOAD_ARG%
if errorlevel 1 (
    echo Release stopped during upload.
    exit /b 1
)

echo.
echo Release completed successfully.
endlocal
