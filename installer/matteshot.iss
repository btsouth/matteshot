; Matteshot installer — per-user (no UAC), tray app.
; Build: ISCC.exe installer\matteshot.iss   (from the repo root)

#ifndef AppVersion
  #define AppVersion "0.9.0"
#endif

[Setup]
AppId={{8B1F3C52-9D14-4A6E-B7E0-52A32C1D9F41}
AppName=Matteshot
AppVersion={#AppVersion}
AppPublisher=SouthForge AI
AppPublisherURL=https://matteshot.app
AppSupportURL=https://matteshot.app
DefaultDirName={localappdata}\Programs\Matteshot
DisableProgramGroupPage=yes
DisableDirPage=yes
PrivilegesRequired=lowest
OutputDir=Output
OutputBaseFilename=MatteshotSetup-{#AppVersion}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
CloseApplications=yes
UninstallDisplayName=Matteshot

[Files]
Source: "..\target\release\matteshot.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\Matteshot"; Filename: "{app}\matteshot.exe"

[Tasks]
Name: "autostart"; Description: "Start Matteshot when Windows starts"; GroupDescription: "Options:"

[Registry]
Root: HKCU; Subkey: "Software\Microsoft\Windows\CurrentVersion\Run"; ValueType: string; ValueName: "Matteshot"; ValueData: """{app}\matteshot.exe"""; Flags: uninsdeletevalue; Tasks: autostart

[Run]
Filename: "{app}\matteshot.exe"; Description: "Launch Matteshot"; Flags: nowait postinstall skipifsilent

[UninstallRun]
; Give PrtScn back to Snipping Tool and stop the resident app.
Filename: "{app}\matteshot.exe"; Parameters: "--restore-printscreen"; Flags: runhidden; RunOnceId: "RestorePrtScn"
Filename: "{cmd}"; Parameters: "/C taskkill /f /im matteshot.exe"; Flags: runhidden; RunOnceId: "KillApp"

[Code]
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  R: Integer;
begin
  // Stop a running instance so the exe can be replaced.
  Exec(ExpandConstant('{cmd}'), '/C taskkill /f /im matteshot.exe', '', SW_HIDE, ewWaitUntilTerminated, R);
  Result := '';
end;
