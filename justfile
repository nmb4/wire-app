set shell := ["powershell.exe", "-NoProfile", "-Command"]

# Build and run wire-app. Extra arguments are passed to the executable.
default: run

build:
    cargo b -r -p wire-app --no-default-features

run *args: build
    & ".\target\release\wire-app.exe" {{ args }}

# Launch the local three-participant development fixture.
dev-pair session="": build
    & ".\target\release\wire-app.exe" --dev-pair {{ session }}

# Increment wire-app's patch or minor version in Cargo.toml and Cargo.lock.
bump-version part="patch":
    #!powershell.exe
    $ErrorActionPreference = "Stop"
    $part = "{{ part }}"
    if ($part -notin @("patch", "minor")) {
        throw "Usage: just bump-version [patch|minor]"
    }

    $root = (Get-Location).Path
    $manifestPath = Join-Path $root "wire-app\Cargo.toml"
    $lockPath = Join-Path $root "Cargo.lock"
    $content = [IO.File]::ReadAllText($manifestPath)
    $lockContent = [IO.File]::ReadAllText($lockPath)
    $pattern = '(?ms)(^\[package\]\s*.*?^version\s*=\s*")(\d+)\.(\d+)\.(\d+)(")'
    $match = [regex]::Match($content, $pattern)
    if (-not $match.Success) {
        throw "Could not find a major.minor.patch package version in $manifestPath"
    }

    $major = [int]$match.Groups[2].Value
    $minor = [int]$match.Groups[3].Value
    $patch = [int]$match.Groups[4].Value
    $oldVersion = "$major.$minor.$patch"
    if ($part -eq "minor") {
        $minor++
        $patch = 0
    } else {
        $patch++
    }
    $newVersion = "$major.$minor.$patch"

    $replacement = $match.Groups[1].Value + $newVersion + $match.Groups[5].Value
    $updated = $content.Substring(0, $match.Index) + $replacement + $content.Substring($match.Index + $match.Length)
    $lockPattern = '(?ms)(^\[\[package\]\]\s*^name\s*=\s*"wire-app"\s*^version\s*=\s*")([^"]+)(")'
    $lockMatch = [regex]::Match($lockContent, $lockPattern)
    if (-not $lockMatch.Success -or $lockMatch.Groups[2].Value -ne $oldVersion) {
        throw "Cargo.lock does not contain wire-app version $oldVersion"
    }
    $lockReplacement = $lockMatch.Groups[1].Value + $newVersion + $lockMatch.Groups[3].Value
    $updatedLock = $lockContent.Substring(0, $lockMatch.Index) + $lockReplacement + $lockContent.Substring($lockMatch.Index + $lockMatch.Length)
    $utf8WithoutBom = [Text.UTF8Encoding]::new($false)

    try {
        [IO.File]::WriteAllText($manifestPath, $updated, $utf8WithoutBom)
        [IO.File]::WriteAllText($lockPath, $updatedLock, $utf8WithoutBom)
    } catch {
        [IO.File]::WriteAllText($manifestPath, $content, $utf8WithoutBom)
        [IO.File]::WriteAllText($lockPath, $lockContent, $utf8WithoutBom)
        throw
    }
    Write-Host "Bumped wire-app from $oldVersion to $newVersion ($part)."

# Copy the release executable to dist and create a versioned zip archive.
package: build
    #!powershell.exe
    $ErrorActionPreference = "Stop"
    $metadata = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
    $version = ($metadata.packages | Where-Object name -eq "wire-app").version
    if (-not $version) {
        throw "Failed to read the wire-app version from Cargo.toml."
    }

    $root = (Get-Location).Path
    $artifactDir = Join-Path $root "dist"
    $exePath = Join-Path $artifactDir "wire-app.exe"
    $zipPath = Join-Path $artifactDir "wire-app-v$version.zip"
    New-Item -ItemType Directory -Force $artifactDir | Out-Null
    Copy-Item -Force (Join-Path $root "target\release\wire-app.exe") $exePath

    for ($attempt = 1; $attempt -le 5; $attempt++) {
        try {
            Compress-Archive -LiteralPath $exePath -DestinationPath $zipPath -Force -ErrorAction Stop
            break
        } catch {
            if ($attempt -eq 5) { throw }
            Start-Sleep -Milliseconds 500
        }
    }
    if (-not (Test-Path -LiteralPath $zipPath)) {
        throw "Failed to create release archive: $zipPath"
    }
    Write-Host "Created $exePath and $zipPath."

# Upload the current versioned zip. Pass --dry-run to only print the action.
upload dry_run="":
    #!powershell.exe
    $ErrorActionPreference = "Stop"
    $dryRun = "{{ dry_run }}"
    if ($dryRun -notin @("", "--dry-run")) {
        throw "Usage: just upload [--dry-run]"
    }

    $metadata = cargo metadata --no-deps --format-version 1 | ConvertFrom-Json
    $version = ($metadata.packages | Where-Object name -eq "wire-app").version
    $root = (Get-Location).Path
    $zipPath = Join-Path $root "dist\wire-app-v$version.zip"
    $uploadUrl = "https://api.stardive.space/v1/files"
    if (-not (Test-Path -LiteralPath $zipPath)) {
        throw "Release archive not found: $zipPath. Run 'just package' first."
    }
    if ($dryRun -eq "--dry-run") {
        Write-Host "Would upload $zipPath to $uploadUrl."
        exit 0
    }

    $responseFile = Join-Path ([IO.Path]::GetTempPath()) "wire-upload-$([guid]::NewGuid().ToString('N')).json"
    try {
        Write-Host "Uploading $zipPath..."
        & curl.exe --silent --show-error --fail-with-body -F "file=@$zipPath;type=application/zip" -o $responseFile $uploadUrl
        if ($LASTEXITCODE -ne 0) {
            if (Test-Path -LiteralPath $responseFile) { Get-Content -Raw $responseFile | Write-Error }
            throw "Upload failed with exit code $LASTEXITCODE."
        }
        $response = Get-Content -Raw $responseFile | ConvertFrom-Json
        $fileId = if ($response.id) { $response.id } elseif ($response.file.id) { $response.file.id } else { $null }
        if (-not $fileId) {
            throw "Upload completed, but the API response did not contain a file ID: $($response | ConvertTo-Json -Depth 10)"
        }
        Write-Host "Upload complete."
        Write-Host "File ID: $fileId"
        Write-Host "Download: $uploadUrl/$fileId"
    } finally {
        Remove-Item -LiteralPath $responseFile -Force -ErrorAction SilentlyContinue
    }

# Bump, package, and upload. Example: just release minor --dry-run
release part="patch" dry_run="":
    just bump-version {{ part }}
    just package
    just upload {{ dry_run }}
