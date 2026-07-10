param(
    [Parameter(Position = 0)]
    [ValidateSet("patch", "minor")]
    [string]$Part = "patch"
)

$ErrorActionPreference = "Stop"
$manifestPath = Join-Path $PSScriptRoot "wire-app\Cargo.toml"
$lockPath = Join-Path $PSScriptRoot "Cargo.lock"
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

if ($Part -eq "minor") {
    $minor++
    $patch = 0
} else {
    $patch++
}

$newVersion = "$major.$minor.$patch"
$replacement = $match.Groups[1].Value + $newVersion + $match.Groups[5].Value
$updated = $content.Substring(0, $match.Index) + $replacement + $content.Substring($match.Index + $match.Length)
$lockPattern = '(?ms)(^\[\[package\]\]\s*^name\s*=\s*"wire-app"\s*^version\s*=\s*")([^\"]+)(")'
$lockMatch = [regex]::Match($lockContent, $lockPattern)
if (-not $lockMatch.Success -or $lockMatch.Groups[2].Value -ne $oldVersion) {
    throw "Cargo.lock does not contain wire-app version $oldVersion"
}
$lockReplacement = $lockMatch.Groups[1].Value + $newVersion + $lockMatch.Groups[3].Value
$updatedLock = $lockContent.Substring(0, $lockMatch.Index) + $lockReplacement + $lockContent.Substring($lockMatch.Index + $lockMatch.Length)
$utf8WithoutBom = New-Object Text.UTF8Encoding($false)

try {
    [IO.File]::WriteAllText($manifestPath, $updated, $utf8WithoutBom)
    [IO.File]::WriteAllText($lockPath, $updatedLock, $utf8WithoutBom)
} catch {
    [IO.File]::WriteAllText($manifestPath, $content, $utf8WithoutBom)
    [IO.File]::WriteAllText($lockPath, $lockContent, $utf8WithoutBom)
    throw
}

Write-Host "Bumped wire-app from $oldVersion to $newVersion ($Part)."