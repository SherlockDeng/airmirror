[CmdletBinding()]
param(
    [switch]$NoLaunch
)

$ErrorActionPreference = "Stop"

$appName = "AirMirror"
$sourceDir = $PSScriptRoot
$installDir = Join-Path $env:LOCALAPPDATA "Programs\AirMirror"
$guiPath = Join-Path $installDir "airmirror-gui.exe"
$startMenuDir = Join-Path $env:APPDATA "Microsoft\Windows\Start Menu\Programs"
$shortcutPath = Join-Path $startMenuDir "AirMirror.lnk"

$running = Get-Process -Name "airmirror-gui", "airmirror" -ErrorAction SilentlyContinue
if ($running) {
    throw "AirMirror is running. Close it before installing or upgrading."
}

New-Item -ItemType Directory -Path $installDir -Force | Out-Null
Copy-Item -Path (Join-Path $sourceDir "*") -Destination $installDir -Recurse -Force

$shell = New-Object -ComObject WScript.Shell
$shortcut = $shell.CreateShortcut($shortcutPath)
$shortcut.TargetPath = $guiPath
$shortcut.WorkingDirectory = $installDir
$shortcut.IconLocation = $guiPath
$shortcut.Description = "AirPlay screen mirroring for Windows"
$shortcut.Save()

$registryPath = "HKCU:\Software\Microsoft\Windows\CurrentVersion\Uninstall\AirMirror"
New-Item -Path $registryPath -Force | Out-Null
Set-ItemProperty -Path $registryPath -Name DisplayName -Value $appName
Set-ItemProperty -Path $registryPath -Name DisplayVersion -Value "0.4.0"
Set-ItemProperty -Path $registryPath -Name Publisher -Value "AirMirror contributors"
Set-ItemProperty -Path $registryPath -Name InstallLocation -Value $installDir
Set-ItemProperty -Path $registryPath -Name DisplayIcon -Value $guiPath
Set-ItemProperty -Path $registryPath -Name UninstallString -Value (
    "powershell.exe -NoProfile -ExecutionPolicy Bypass -File `"$installDir\uninstall.ps1`""
)
Set-ItemProperty -Path $registryPath -Name NoModify -Type DWord -Value 1
Set-ItemProperty -Path $registryPath -Name NoRepair -Type DWord -Value 1

Write-Host "AirMirror installed to: $installDir"

if (-not $NoLaunch) {
    Start-Process -FilePath $guiPath -WorkingDirectory $installDir
}
