; NebulaDisplay Windows installer (Inno Setup 6).
; Build:  iscc installer\windows\nebuladisplay.iss
; Inputs (relative to repo root):
;   target\x86_64-pc-windows-msvc\release\nebula-host.exe
;   viewer\web\dist\**            (built web UI)
;   host\tray-ui\NebulaDisplayTray.exe
;   host\windows-driver\x64\Release\NebulaDisplay\**   (optional driver pkg)

#define AppName "NebulaDisplay"
#define AppVersion "0.1.0"
#define AppPublisher "NebulaDisplay Project"

[Setup]
AppId={{6E2B7C51-2D3A-4F0B-9E1C-1F2A3B4C5D6E}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
OutputBaseFilename=NebulaDisplay-Setup-{#AppVersion}
Compression=lzma2
SolidCompression=yes
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=admin
UninstallDisplayIcon={app}\NebulaDisplayTray.exe
WizardStyle=modern

[Files]
Source: "..\..\target\x86_64-pc-windows-msvc\release\nebula-host.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\viewer\web\dist\*"; DestDir: "{app}\web"; Flags: ignoreversion recursesubdirs
Source: "..\..\host\tray-ui\NebulaDisplayTray.exe"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "..\..\host\windows-driver\x64\Release\NebulaDisplay\*"; DestDir: "{app}\driver"; Flags: ignoreversion recursesubdirs skipifsourcedoesntexist
Source: "install-driver.ps1"; DestDir: "{app}\scripts"; Flags: ignoreversion
Source: "uninstall-driver.ps1"; DestDir: "{app}\scripts"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName} control panel"; Filename: "https://localhost:38470/"
Name: "{group}\{#AppName} tray"; Filename: "{app}\NebulaDisplayTray.exe"
Name: "{userstartup}\{#AppName} tray"; Filename: "{app}\NebulaDisplayTray.exe"; Tasks: traystartup

[Tasks]
Name: "traystartup"; Description: "Start the tray companion when I sign in"
Name: "installdriver"; Description: "Install the virtual display driver (extend mode; requires driver package + signing, see docs)"; Flags: unchecked

[Run]
; Register + start the host service.
Filename: "{sys}\sc.exe"; Parameters: "create NebulaDisplayHost binPath= ""\""{app}\nebula-host.exe\"" --service --web-dir \""{app}\web\"""" start= auto DisplayName= ""NebulaDisplay Host"""; Flags: runhidden
Filename: "{sys}\sc.exe"; Parameters: "description NebulaDisplayHost ""Streams this PC's display to NebulaDisplay viewers on the local network."""; Flags: runhidden
; Windows Firewall: allow the streaming port + discovery on private networks only.
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall add rule name=""NebulaDisplay Host"" dir=in action=allow program=""{app}\nebula-host.exe"" profile=private,domain protocol=tcp localport=38470"; Flags: runhidden
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall add rule name=""NebulaDisplay Discovery"" dir=in action=allow program=""{app}\nebula-host.exe"" profile=private,domain protocol=udp localport=38471"; Flags: runhidden
Filename: "{sys}\sc.exe"; Parameters: "start NebulaDisplayHost"; Flags: runhidden
Filename: "powershell.exe"; Parameters: "-ExecutionPolicy Bypass -File ""{app}\scripts\install-driver.ps1"" -DriverDir ""{app}\driver"""; Tasks: installdriver; Flags: runhidden
Filename: "{app}\NebulaDisplayTray.exe"; Description: "Launch tray companion"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{sys}\sc.exe"; Parameters: "stop NebulaDisplayHost"; Flags: runhidden; RunOnceId: "svcstop"
Filename: "{sys}\sc.exe"; Parameters: "delete NebulaDisplayHost"; Flags: runhidden; RunOnceId: "svcdel"
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""NebulaDisplay Host"""; Flags: runhidden; RunOnceId: "fw1"
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""NebulaDisplay Discovery"""; Flags: runhidden; RunOnceId: "fw2"
Filename: "powershell.exe"; Parameters: "-ExecutionPolicy Bypass -File ""{app}\scripts\uninstall-driver.ps1"" -Quiet"; Flags: runhidden; RunOnceId: "drv"
