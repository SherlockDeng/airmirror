[CmdletBinding()]
param()

$ErrorActionPreference = "Stop"

$installDir = Join-Path $env:LOCALAPPDATA "Programs\AirMirror"
$startMenuShortcut = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs\AirMirror.lnk"
$registryPath = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\AirMirror"

Get-Process -Name "airmirror-gui", "airmirror" -ErrorAction SilentlyContinue | Stop-Process -Force

$ffmpegPath = Join-Path $installDir "ffmpeg.exe"
Get-Process -Name "ffmpeg" -ErrorAction SilentlyContinue |
    Where-Object { $_.Path -eq $ffmpegPath } |
    Stop-Process -Force

Remove-Item -LiteralPath $startMenuShortcut -Force -ErrorAction SilentlyContinue
Remove-Item -LiteralPath $registryPath -Recurse -Force -ErrorAction SilentlyContinue

Set-Location $env:TEMP
Start-Sleep -Milliseconds 300
if (Test-Path -LiteralPath $installDir) {
    Remove-Item -LiteralPath $installDir -Recurse -Force
}

Write-Host "AirMirror has been removed."
