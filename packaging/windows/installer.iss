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
Name: "controllers"; Description: "Game controllers, including the full DualSense (ViGEmBus, usbip-win2)"; GroupDescription: "Free add-ons, downloaded from their makers:"
Name: "mic"; Description: "Microphone from your other computer (VB-CABLE)"; GroupDescription: "Free add-ons, downloaded from their makers:"
Name: "tailscale"; Description: "Reach this PC from anywhere (Tailscale)"; GroupDescription: "Free add-ons, downloaded from their makers:"

[Files]
Source: "..\..\target\release\anywhere.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\target\release\aa-host.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\target\release\aa-viewer.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "addons.ps1"; DestDir: "{tmp}"; Flags: deleteafterinstall

[Icons]
Name: "{group}\Anywhere"; Filename: "{app}\anywhere.exe"
Name: "{autodesktop}\Anywhere"; Filename: "{app}\anywhere.exe"; Tasks: desktopicon

[Run]
; Let other computers reach this one (sharing) and let discovery answers in,
; so Windows never interrupts the first connection with a firewall prompt.
; (Delete first: every update would otherwise add the rules again.)
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""Anywhere host"""; Flags: runhidden
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""Anywhere viewer"""; Flags: runhidden
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""Anywhere app"""; Flags: runhidden
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall add rule name=""Anywhere host"" dir=in action=allow program=""{app}\aa-host.exe"" enable=yes"; Flags: runhidden
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall add rule name=""Anywhere viewer"" dir=in action=allow program=""{app}\aa-viewer.exe"" enable=yes"; Flags: runhidden
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall add rule name=""Anywhere app"" dir=in action=allow program=""{app}\anywhere.exe"" enable=yes"; Flags: runhidden
Filename: "powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{tmp}\addons.ps1"" {code:AddonList}"; StatusMsg: "Installing the free add-ons (this can take a minute)..."; Flags: runhidden waituntilterminated; Check: WantAddons
; If "share this PC at all times" was on, start that service again with the new files.
Filename: "{sys}\sc.exe"; Parameters: "start AnywhereHost"; Flags: runhidden
Filename: "{app}\anywhere.exe"; Description: "Open Anywhere"; Flags: nowait postinstall skipifsilent
; An automatic update started by the app (/RELAUNCH=1): reopen it for the user.
Filename: "{app}\anywhere.exe"; Parameters: "--after-update"; Flags: nowait runasoriginaluser; Check: Relaunch

[UninstallRun]
Filename: "{sys}\sc.exe"; Parameters: "stop AnywhereHost"; Flags: runhidden; RunOnceId: "svcstop"
Filename: "{sys}\sc.exe"; Parameters: "delete AnywhereHost"; Flags: runhidden; RunOnceId: "svcdelete"
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""Anywhere host"""; Flags: runhidden; RunOnceId: "fwhost"
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""Anywhere viewer"""; Flags: runhidden; RunOnceId: "fwviewer"
Filename: "{sys}\netsh.exe"; Parameters: "advfirewall firewall delete rule name=""Anywhere app"""; Flags: runhidden; RunOnceId: "fwapp"

[UninstallDelete]
Type: filesandordirs; Name: "{userappdata}\AnywhereAlternative"
Type: filesandordirs; Name: "{commonappdata}\AnywhereAlternative"

[Code]
// The ticked add-ons as words for addons.ps1 ('' when none).
function AddonList(Param: String): String;
begin
  Result := '';
  if WizardIsTaskSelected('controllers') then Result := Result + ' controllers';
  if WizardIsTaskSelected('mic') then Result := Result + ' mic';
  if WizardIsTaskSelected('tailscale') then Result := Result + ' tailscale';
  Result := Trim(Result);
end;

function WantAddons: Boolean;
begin
  Result := AddonList('') <> '';
end;

function Relaunch: Boolean;
begin
  Result := ExpandConstant('{param:RELAUNCH|0}') = '1';
end;

// The always-on sharing service keeps aa-host.exe open; stop it so the
// update can replace the file ([Run] starts it again afterwards).
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  Code: Integer;
begin
  Exec(ExpandConstant('{sys}\sc.exe'), 'stop AnywhereHost', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Sleep(4000);
  Result := '';
end;
