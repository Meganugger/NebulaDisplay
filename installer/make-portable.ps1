# Builds a portable, admin-free NebulaDisplay bundle (host + viewer + web UI).
# Run from the repo root on Windows after:
#   cargo build --release
#   cd viewer/web && npm install && npm run build
# Output: dist/NebulaDisplay-portable-<version>.zip

$ErrorActionPreference = "Stop"
$version = (Select-String -Path Cargo.toml -Pattern '^version = "(.+)"').Matches[0].Groups[1].Value
$stage = "dist/portable"
Remove-Item -Recurse -Force $stage -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force "$stage/web" | Out-Null

Copy-Item target/release/nebulad.exe $stage/
Copy-Item target/release/nebula-tray.exe $stage/ -ErrorAction SilentlyContinue
Copy-Item target/release/nebula-viewer.exe $stage/ -ErrorAction SilentlyContinue
Copy-Item -Recurse viewer/web/dist/* "$stage/web/"

@"
NebulaDisplay portable bundle v$version
======================================

Host (the PC whose screen you extend/mirror):
  nebulad.exe            starts the host; prints viewer URLs + pairing PIN
                         (control panel: http://127.0.0.1:41888/panel.html)
  nebula-tray.exe        optional tray icon (starts nebulad for you)

Viewer (the extra-screen device):
  * any browser:         open the URL nebulad prints, enter the PIN
  * nebula-viewer.exe    native viewer: nebula-viewer --host <ip>:41800 --pin <pin>

No installation or admin rights required. Windows Firewall may prompt once to
allow nebulad on private networks — that's the viewer port (TCP 41800).
Extend mode (true virtual monitor) needs the driver package; see
host/windows-driver/README.md in the source repository.
"@ | Set-Content "$stage/README.txt"

Compress-Archive -Path "$stage/*" -DestinationPath "dist/NebulaDisplay-portable-$version.zip" -Force
Write-Host "Created dist/NebulaDisplay-portable-$version.zip"
