[CmdletBinding()]
param(
    [string]$Version = "",
    [string]$FfmpegPath = "",
    [switch]$SkipBuild,
    [switch]$NoFfmpeg
)

$ErrorActionPreference = "Stop"

$root = (Resolve-Path (Join-Path $PSScriptRoot "..")).Path
$targetRelease = Join-Path $root "target\release"
$targetPackage = Join-Path $root "target\package"
$workspaceToml = Get-Content (Join-Path $root "Cargo.toml") -Raw
if (-not $Version) {
    $versionMatch = [regex]::Match($workspaceToml, '(?m)^version\s*=\s*"([^"]+)"')
    if (-not $versionMatch.Success) {
        throw "Cannot determine workspace version from Cargo.toml"
    }
    $Version = $versionMatch.Groups[1].Value
}

$packageName = "AirMirror-v$Version-windows-x64"
$stage = Join-Path $targetPackage "stage-$packageName"
$archive = Join-Path $targetPackage "$packageName.zip"
$destinationDir = Join-Path $root "packages\v$Version\windows-x64"
$destinationArchive = Join-Path $destinationDir "$packageName.zip"

if (-not $SkipBuild) {
    $env:RUSTUP_HOME = if ($env:RUSTUP_HOME) { $env:RUSTUP_HOME } else { "D:\dev\rustup" }
    $env:CARGO_HOME = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { "D:\dev\cargo" }
    cargo build --release --bin airmirror --bin airmirror-gui --bin airmirror-receiver-info
    if ($LASTEXITCODE -ne 0) {
        throw "Cargo release build failed"
    }

    Push-Location (Join-Path $root "tools\fpsap-helper")
    try {
        go build -trimpath -o (Join-Path $targetRelease "fpsap-helper.exe") .
        if ($LASTEXITCODE -ne 0) {
            throw "fpsap-helper build failed"
        }
    }
    finally {
        Pop-Location
    }
}

$required = @(
    "airmirror.exe",
    "airmirror-gui.exe",
    "airmirror-receiver-info.exe",
    "fpsap-helper.exe"
)
foreach ($name in $required) {
    if (-not (Test-Path (Join-Path $targetRelease $name))) {
        throw "Missing release file: target\release\$name"
    }
}

New-Item -ItemType Directory -Path $targetPackage -Force | Out-Null
if (Test-Path -LiteralPath $stage) {
    $resolvedStage = (Resolve-Path -LiteralPath $stage).Path
    if (-not $resolvedStage.StartsWith($targetPackage, [System.StringComparison]::OrdinalIgnoreCase)) {
        throw "Unsafe staging path: $resolvedStage"
    }
    Remove-Item -LiteralPath $resolvedStage -Recurse -Force
}
New-Item -ItemType Directory -Path $stage -Force | Out-Null

foreach ($name in $required) {
    Copy-Item -LiteralPath (Join-Path $targetRelease $name) -Destination $stage
}
Copy-Item -Path (Join-Path $root "packaging\windows\*") -Destination $stage -Recurse
Copy-Item -LiteralPath (Join-Path $root "LICENSE") -Destination $stage
Copy-Item -LiteralPath (Join-Path $root "THIRD_PARTY_NOTICES.md") -Destination $stage

if (-not $NoFfmpeg) {
    if ($FfmpegPath) {
        $resolvedFfmpeg = (Resolve-Path -LiteralPath $FfmpegPath).Path
    }
    else {
        $ffmpegCommand = Get-Command ffmpeg -ErrorAction Stop
        $ffmpegItem = Get-Item -LiteralPath $ffmpegCommand.Source
        $resolvedFfmpeg = if ($ffmpegItem.LinkType) { $ffmpegItem.Target } else { $ffmpegItem.FullName }
    }
    Copy-Item -LiteralPath $resolvedFfmpeg -Destination (Join-Path $stage "ffmpeg.exe")

    $ffmpegDir = Split-Path $resolvedFfmpeg -Parent
    $licenseCandidates = @(
        (Join-Path $ffmpegDir "LICENSE"),
        (Join-Path (Split-Path $ffmpegDir -Parent) "LICENSE")
    )
    $license = $licenseCandidates | Where-Object { Test-Path -LiteralPath $_ } | Select-Object -First 1
    if ($license) {
        Copy-Item -LiteralPath $license -Destination (Join-Path $stage "FFmpeg-LICENSE.txt")
    }
}

$versionInfo = @(
    "AirMirror $Version",
    "Windows x64",
    "Built: $((Get-Date).ToUniversalTime().ToString('yyyy-MM-ddTHH:mm:ssZ'))"
)
Set-Content -LiteralPath (Join-Path $stage "VERSION.txt") -Value $versionInfo -Encoding utf8

if (Test-Path -LiteralPath $archive) {
    Remove-Item -LiteralPath $archive -Force
}
Add-Type -AssemblyName System.IO.Compression.FileSystem
[System.IO.Compression.ZipFile]::CreateFromDirectory(
    $stage,
    $archive,
    [System.IO.Compression.CompressionLevel]::SmallestSize,
    $false
)

$size = (Get-Item -LiteralPath $archive).Length
$githubFileLimit = 100 * 1024 * 1024
if ($size -ge $githubFileLimit) {
    throw "Archive is $size bytes, which exceeds GitHub's 100 MiB file limit"
}

New-Item -ItemType Directory -Path $destinationDir -Force | Out-Null
Copy-Item -LiteralPath $archive -Destination $destinationArchive -Force
$hash = (Get-FileHash -LiteralPath $destinationArchive -Algorithm SHA256).Hash.ToLowerInvariant()
"$hash  $packageName.zip" | Set-Content -LiteralPath (Join-Path $destinationDir "SHA256SUMS.txt") -Encoding ascii

Write-Host "Created: $destinationArchive"
Write-Host "SHA256:  $hash"
Write-Host "Size:    $size bytes"
