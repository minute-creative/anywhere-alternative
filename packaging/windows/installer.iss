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
; Don't let Windows' Restart Manager hunt for programs using our files: the
; camera DLL is loaded by Windows' shared camera service, and asking that to
; close can hang a silent install with nothing on screen (the PC's
; automatic updates stalled three times, 0.5.9 to 0.5.11). Our own programs
; are closed in PrepareToInstall below; the DLL is swapped on restart
; (restartreplace).
CloseApplications=no
RestartApplications=no

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Shortcuts:"
Name: "controllers"; Description: "Game controllers, including the full DualSense (ViGEmBus, usbip-win2)"; GroupDescription: "Free add-ons, downloaded from their makers:"
Name: "mic"; Description: "Microphone from your other computer (VB-CABLE)"; GroupDescription: "Free add-ons, downloaded from their makers:"
Name: "tailscale"; Description: "Reach this PC from anywhere (Tailscale)"; GroupDescription: "Free add-ons, downloaded from their makers:"
Name: "hevc"; Description: "Sharper picture for less data (Microsoft's HEVC Video Extensions)"; GroupDescription: "Free add-ons, downloaded from their makers:"

[Files]
Source: "..\..\target\release\anywhere.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\target\release\aa-host.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "..\..\target\release\aa-viewer.exe"; DestDir: "{app}"; Flags: ignoreversion
; "Anywhere Camera" for every app (Windows 11): loaded by Windows' camera service.
Source: "..\..\target\release\aa_vcam.dll"; DestDir: "{app}"; Flags: ignoreversion restartreplace uninsrestartdelete
Source: "addons.ps1"; DestDir: "{tmp}"; Flags: deleteafterinstall
; The browser add-on for Chrome and Edge ("Load unpacked" until it is in the stores).
Source: "..\..\dist\addon\chrome\*"; DestDir: "{app}\browser-addon"; Flags: ignoreversion recursesubdirs createallsubdirs

[Registry]
; Tell Windows where the camera DLL is (its class id is in aa-core/src/calls.rs).
Root: HKLM; Subkey: "Software\Classes\CLSID\{{4ABA54B9-4410-46E2-99D7-5445CD96A471}"; ValueType: string; ValueData: "Anywhere Camera"; Flags: uninsdeletekey
Root: HKLM; Subkey: "Software\Classes\CLSID\{{4ABA54B9-4410-46E2-99D7-5445CD96A471}\InprocServer32"; ValueType: string; ValueData: "{app}\aa_vcam.dll"
Root: HKLM; Subkey: "Software\Classes\CLSID\{{4ABA54B9-4410-46E2-99D7-5445CD96A471}\InprocServer32"; ValueType: string; ValueName: "ThreadingModel"; ValueData: "Both"

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
Filename: "{sys}\schtasks.exe"; Parameters: "/Delete /F /TN AnywhereUpdateGuard"; Flags: runhidden; RunOnceId: "guarddelete"
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
  if WizardIsTaskSelected('hevc') then Result := Result + ' hevc';
  Result := Trim(Result);
end;

function WantAddons: Boolean;
begin
  // Not during automatic (silent) updates: the add-on downloads can wait for
  // a question nobody sees, and the update then never finishes (0.5.3).
  Result := (AddonList('') <> '') and not WizardSilent;
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
  // Anything of ours still running holds its .exe open: end it, so the
  // files can be replaced without asking anyone (the app reopens itself
  // after an automatic update, see [Run]).
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM anywhere.exe', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM aa-viewer.exe', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Exec(ExpandConstant('{sys}\taskkill.exe'), '/F /IM aa-host.exe', '', SW_HIDE, ewWaitUntilTerminated, Code);
  Sleep(1000);
  Result := '';
end;
