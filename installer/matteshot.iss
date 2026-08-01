; Matteshot installer — per-user (no UAC), tray app.
; Build: ISCC.exe installer\matteshot.iss   (from the repo root)

#ifndef AppVersion
  #define AppVersion "0.10.0"
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
SetupIconFile=..\assets\matteshot.ico
UninstallDisplayIcon={app}\matteshot.exe

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
; Stop through Matteshot's own cleanup path, then restore the Windows binding.
Filename: "{app}\matteshot.exe"; Parameters: "--quit"; Flags: runhidden waituntilterminated; RunOnceId: "StopApp"
Filename: "{app}\matteshot.exe"; Parameters: "--restore-printscreen"; Flags: runhidden waituntilterminated; RunOnceId: "RestorePrtScn"

[Code]
function PrepareToInstall(var NeedsRestart: Boolean): String;
var
  R: Integer;
begin
  // Never force-kill an active recording or export. Matteshot closes its UI
  // surfaces, waits for their cleanup paths, then exits the resident loop.
  if FileExists(ExpandConstant('{app}\matteshot.exe')) then begin
    if not Exec(ExpandConstant('{app}\matteshot.exe'), '--quit', '', SW_HIDE,
      ewWaitUntilTerminated, R) then begin
      Result := 'Matteshot could not be closed. Close it from the tray and try again.';
      exit;
    end;
    if R <> 0 then
      // Older Matteshot builds do not know --quit. Ask Windows to close them
      // without /f; CloseApplications remains the final file-lock safeguard.
      Exec(ExpandConstant('{cmd}'), '/C taskkill /im matteshot.exe', '', SW_HIDE,
        ewWaitUntilTerminated, R);
  end;
  Result := '';
end;
