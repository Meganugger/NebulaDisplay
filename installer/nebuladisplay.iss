; NebulaDisplay Windows installer (Inno Setup 6).
; Build:  iscc installer\nebuladisplay.iss
; Inputs (place next to this script or pass /D defines):
;   nebulad.exe, nebula-tray.exe, nebula-viewer.exe  (cargo build --release)
;   web\  (viewer/web/dist)
;   driver\  (optional signed driver package + nebula-devnode.exe)

#define AppName "NebulaDisplay"
#define AppVersion "0.2.0"
#define AppPublisher "NebulaDisplay Project"

[Setup]
AppId={{7C3E9A44-51D2-4B7B-A46B-0E64F1B7C0D9}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
OutputBaseFilename=NebulaDisplay-Setup-{#AppVersion}
Compression=lzma2/max
SolidCompression=yes
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=admin
UninstallDisplayName={#AppName} Host
WizardStyle=modern

[Tasks]
Name: "autostart"; Description: "Start the NebulaDisplay tray on sign-in"; GroupDescription: "Extras:"
Name: "firewall"; Description: "Add Windows Firewall rules (viewer port + discovery)"; GroupDescription: "Extras:"
Name: "driver"; Description: "Install the virtual display driver (extend mode; requires a signed package)"; GroupDescription: "Extras:"; Flags: unchecked

[Files]
Source: "nebulad.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "nebula-tray.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "nebula-viewer.exe"; DestDir: "{app}"; Flags: ignoreversion skipifsourcedoesntexist
Source: "web\*"; DestDir: "{app}\web"; Flags: ignoreversion recursesubdirs
Source: "driver\*"; DestDir: "{app}\driver"; Flags: ignoreversion recursesubdirs skipifsourcedoesntexist
Source: "install-driver.ps1"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\{#AppName} Host"; Filename: "{app}\nebula-tray.exe"
Name: "{group}\{#AppName} Control Panel"; Filename: "http://127.0.0.1:41888/panel.html"
Name: "{group}\Uninstall {#AppName}"; Filename: "{uninstallexe}"

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; \
  ValueName: "NebulaDisplayTray"; ValueData: """{app}\nebula-tray.exe"""; \
  Flags: uninsdeletevalue; Tasks: autostart

[Run]
; Firewall: viewer WS/HTTP (TCP 41800) + discovery (UDP 41799), private profile only.
Filename: "netsh"; Parameters: "advfirewall firewall add rule name=""NebulaDisplay Viewer"" dir=in action=allow program=""{app}\nebulad.exe"" protocol=TCP localport=41800 profile=private"; Flags: runhidden; Tasks: firewall
Filename: "netsh"; Parameters: "advfirewall firewall add rule name=""NebulaDisplay Discovery"" dir=in action=allow program=""{app}\nebulad.exe"" protocol=UDP localport=41799 profile=private"; Flags: runhidden; Tasks: firewall
Filename: "powershell"; Parameters: "-ExecutionPolicy Bypass -File ""{app}\install-driver.ps1"" -DriverDir ""{app}\driver"""; Flags: runhidden; Tasks: driver
Filename: "{app}\nebula-tray.exe"; Description: "Start {#AppName} now"; Flags: postinstall nowait skipifsilent

[UninstallRun]
Filename: "netsh"; Parameters: "advfirewall firewall delete rule name=""NebulaDisplay Viewer"""; Flags: runhidden; RunOnceId: "fw1"
Filename: "netsh"; Parameters: "advfirewall firewall delete rule name=""NebulaDisplay Discovery"""; Flags: runhidden; RunOnceId: "fw2"
Filename: "powershell"; Parameters: "-ExecutionPolicy Bypass -File ""{app}\install-driver.ps1"" -Uninstall"; Flags: runhidden; RunOnceId: "drv"
