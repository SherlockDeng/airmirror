# AirMirror v0.4.0 - Windows x64

Download `AirMirror-v0.4.0-windows-x64.zip`, verify it against
`SHA256SUMS.txt`, then extract and run `install.cmd`.

The package contains:

- `airmirror-gui.exe`
- `airmirror.exe`
- `airmirror-receiver-info.exe`
- `fpsap-helper.exe`
- FFmpeg 8.1.1 with `ddagrab` and `h264_nvenc`
- install and uninstall scripts
- license and third-party notices

Requirements:

- Windows 10/11 x64
- AirPlay receiver reachable on the same LAN
- NVIDIA GPU for 2K/4K high-refresh hardware encoding
- Windows Firewall access for private networks

System audio uses WASAPI loopback. Pairing does not require a PIN. User
configuration and credentials remain under `%APPDATA%\airmirror` after uninstall.
