; Inno Setup script for the Windows installer.
; Built by the release workflow:  iscc /DAppVersion=x.y.z packaging\windows\installer.iss

#ifndef AppVersion
  #define AppVersion "0.1.0"
#endif

[Setup]
AppId={{6E6B8C2A-5B0E-4E7D-9C59-3A1F2C8D7E41}
AppName=Anywhere
AppVersion={#AppVersion}
AppPublisher=Minute Creative
DefaultDirName={autopf}\Anywhere
DefaultGroupName=Anywhere
OutputDir=..\..\dist
OutputBaseFilename=Anywhere-{#AppVersion}-windows-setup
SetupIconFile=..\..\assets\icon.ico
UninstallDisplayIcon={app}\anywhere.exe
Compression=lzma2
SolidCompression=yes
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
PrivilegesRequired=admin
WizardStyle=modern

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Shortcuts:"

[Files]
Source: "..\..\target\release\anywhere.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\target\release\aa-host.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\target\release\aa-viewer.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{group}\Anywhere"; Filename: "{app}\anywhere.exe"
Name: "{autodesktop}\Anywhere"; Filename: "{app}\anywhere.exe"; Tasks: desktopicon

[Run]
; Let other computers reach this one (sharing) and let discovery answers in,
; so Windows never interrupts the first connection with a firewall prompt.
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall add rule name=""Anywhere host"" dir=in action=allow program=""{app}\aa-host.exe"" enable=yes"; Flags: runhidden
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall add rule name=""Anywhere viewer"" dir=in action=allow program=""{app}\aa-viewer.exe"" enable=yes"; Flags: runhidden
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall add rule name=""Anywhere app"" dir=in action=allow program=""{app}\anywhere.exe"" enable=yes"; Flags: runhidden
Filename: "{app}\anywhere.exe"; Description: "Open Anywhere"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""Anywhere host"""; Flags: runhidden; RunOnceId: "fwhost"
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""Anywhere viewer"""; Flags: runhidden; RunOnceId: "fwviewer"
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""Anywhere app"""; Flags: runhidden; RunOnceId: "fwapp"

[UninstallDelete]
Type: filesandordirs; Name: "{userappdata}\AnywhereAlternative"
