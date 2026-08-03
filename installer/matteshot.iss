; Matteshot installer — per-user (no UAC), tray app.
; Build: ISCC.exe installer\matteshot.iss   (from the repo root)

#ifndef AppVersion
  #define AppVersion "0.14.1"
#endif

; Release tags may carry a SemVer prerelease suffix (v0.13.2-rc1), which the
; version validator accepts and passes straight through. The Windows
; VERSIONINFO resource only takes dotted numbers, so keep the full string for
; AppVersion and strip the suffix for the resource.
#define NumericVersion AppVersion
#if Pos("-", NumericVersion) > 0
  #define NumericVersion Copy(NumericVersion, 1, Pos("-", NumericVersion) - 1)
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

; Inno leaves most of VERSIONINFO blank unless it is told otherwise, and 0.13.1
; shipped with an empty FileVersion, copyright and original filename. Defender's
; static classifier reads these, and a signed installer with no metadata scores
; worse than an unsigned one with a full record.
VersionInfoVersion={#NumericVersion}
VersionInfoProductVersion={#NumericVersion}
VersionInfoCompany=SouthForge AI
VersionInfoProductName=Matteshot
VersionInfoDescription=Matteshot Setup
VersionInfoCopyright=Copyright (C) 2026 SouthForge AI
; Built as MatteshotSetup-<version>.exe, but published and downloaded under the
; stable name, which is the one worth claiming here.
VersionInfoOriginalFileName=MatteshotSetup.exe

[Files]
Source: "..\target\release\matteshot.exe"; DestDir: "{app}"; Flags: ignoreversion

[Icons]
Name: "{userprograms}\Matteshot"; Filename: "{app}\matteshot.exe"
; Autostart is a Startup-folder shortcut rather than an HKCU\...\Run value.
; Both do the same job, but writing a Run key from a freshly downloaded
; installer is the strongest single feature in Defender's
; Behavior:Win32/Persistence family, and it got 0.13.1 quarantined in the
; field. Matteshot migrates anyone who still has the old Run value on startup.
;
; A silent run is a background self-update, and an update has no business
; rewriting the user's autostart choice: the shortcut is already there from the
; original install, and re-running this section resurrects it for anyone who
; turned autostart off in Settings.
Name: "{userstartup}\Matteshot"; Filename: "{app}\matteshot.exe"; Tasks: autostart; Check: not WizardSilent

[Tasks]
Name: "autostart"; Description: "Start Matteshot when Windows starts"; GroupDescription: "Options:"

[Run]
Filename: "{app}\matteshot.exe"; Description: "Launch Matteshot"; Flags: nowait postinstall skipifsilent
; A silent run is a background self-update: nothing offers to relaunch, so put
; the resident back ourselves or the user silently loses their tray app.
Filename: "{app}\matteshot.exe"; Flags: nowait runhidden; Check: WizardSilent

[UninstallDelete]
; Inno only removes the [Icons] shortcut it created itself, so enabling
; autostart from Settings after an install that skipped it used to leave the
; entry behind pointing at a deleted binary. Sweep the shortcut unconditionally.
Type: files; Name: "{userstartup}\Matteshot.lnk"

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
      // taskkill is run directly: routing it through cmd would flash a console
      // window during an otherwise invisible background update.
      Exec('taskkill.exe', '/im matteshot.exe', '', SW_HIDE,
        ewWaitUntilTerminated, R);
  end;
  Result := '';
end;
