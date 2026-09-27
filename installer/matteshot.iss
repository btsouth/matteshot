; Matteshot installer — per-user (no UAC), tray app.
; Build: ISCC.exe installer\matteshot.iss   (from the repo root)

#ifndef AppVersion
  #define AppVersion "0.21.1"
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
; CI passes /DSign plus an /Ssigntool= command wired to scripts\sign-file.ps1.
; Signing has to happen inside the compiler for the uninstaller's sake:
; unins000.exe is generated here, so a post-build signing pass can only ever
; reach the outer setup executable, and 0.18.0 shipped customers an unsigned
; uninstaller that way. SignTool also signs the setup itself, which replaces
; the old separate post-ISCC signing step. Local unsigned builds omit /DSign.
#ifdef Sign
SignTool=signtool
SignedUninstaller=yes
#endif
AppId={{8B1F3C52-9D14-4A6E-B7E0-52A32C1D9F41}
AppName=Matteshot
AppVersion={#AppVersion}
AppPublisher=Southbound Software
AppPublisherURL=https://matteshot.app
AppSupportURL=https://github.com/btsouth/matteshot/issues
AppUpdatesURL=https://github.com/btsouth/matteshot/releases
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
VersionInfoCompany=Southbound Software
VersionInfoProductName=Matteshot
VersionInfoDescription=Matteshot Setup
VersionInfoCopyright=Copyright (C) 2026 Southbound Software. MIT OR Apache-2.0.
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
; Stop through Matteshot's own cleanup path. On the graceful path --quit is
; enough: the resident hands PrtScn back via release(), which restores the
; prior HKCU binding (SBS-1050) or leaves it alone when we never wrote it. Do
; not follow with --restore-printscreen: that flag force-writes
; PrintScreenKeyForSnippingEnabled=1 in a new process and turns Snipping on for
; anyone who had it off (SBS-1072). The trade is deliberate: when --quit fails
; and StopResident falls back to taskkill, a resident that had written the
; value (the RegisterHotKey fallback or the Settings takeover) exits without
; release() and leaves PrtScn off for Snipping. `matteshot --restore-printscreen`
; is the explicit undo for that case.
Filename: "{app}\matteshot.exe"; Parameters: "--quit"; Flags: runhidden waituntilterminated; RunOnceId: "StopApp"

[Code]
function StopResident: Boolean;
var
  R: Integer;
begin
  // Never force-kill an active recording or export. Matteshot closes its UI
  // surfaces, waits for their cleanup paths, then exits the resident loop.
  Result := True;
  if not FileExists(ExpandConstant('{app}\matteshot.exe')) then
    Exit;
  if not Exec(ExpandConstant('{app}\matteshot.exe'), '--quit', '', SW_HIDE,
    ewWaitUntilTerminated, R) then begin
    Result := False;
    Exit;
  end;
  if R <> 0 then
    // Older Matteshot builds do not know --quit. Ask Windows to close them
    // without /f; CloseApplications remains the final file-lock safeguard.
    // The Windows system directory, never an unqualified name: a decoy
    // taskkill.exe beside the installer must not run (SBS-764). Setup has
    // no ArchitecturesInstallIn64BitMode, so {sys} is SysWOW64 on 64-bit
    // Windows; that ships its own taskkill.exe and is just as
    // system-protected, so the guarantee holds either way. Routing through
    // cmd would flash a console during an otherwise invisible update.
    Exec(ExpandConstant('{sys}\taskkill.exe'), '/im matteshot.exe', '', SW_HIDE,
      ewWaitUntilTerminated, R);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  if not StopResident then begin
    Result := 'Matteshot could not be closed. Close it from the tray and try again.';
    exit;
  end;
  Result := '';
end;

function HistoryMetadataDir: String;
begin
  // dirs::config_dir() on Windows is %APPDATA% (Roaming), which Inno calls {userappdata}.
  Result := ExpandConstant('{userappdata}\matteshot');
end;

function HistoryMetadataPresent: Boolean;
var
  FindRec: TFindRec;
begin
  Result := FileExists(HistoryMetadataDir + '\history.json')
    or FileExists(HistoryMetadataDir + '\history.json.tmp');
  if Result then
    Exit;
  if FindFirst(HistoryMetadataDir + '\history.json.corrupt-*', FindRec) then
  begin
    Result := True;
    FindClose(FindRec);
  end;
end;

function DeleteHistoryMetadata: Boolean;
var
  FindRec: TFindRec;
  Dir: String;
begin
  Result := True;
  Dir := HistoryMetadataDir;
  if FileExists(Dir + '\history.json') and not DeleteFile(Dir + '\history.json') then
    Result := False;
  if FileExists(Dir + '\history.json.tmp') and not DeleteFile(Dir + '\history.json.tmp') then
    Result := False;
  if FindFirst(Dir + '\history.json.corrupt-*', FindRec) then
  try
    repeat
      if not DeleteFile(Dir + '\' + FindRec.Name) then
        Result := False;
    until not FindNext(FindRec);
  finally
    FindClose(FindRec);
  end;
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
begin
  // Offer only. Captures live in the save/video folders and stay there
  // unless the user already deleted them. Silent / unattended uninstall
  // leaves the metadata in place so winget does not delete titles without
  // a human Yes. Quit first: usUninstall runs before [UninstallRun] --quit,
  // and a live record() after Yes would recreate history.json. SBS-765.
  if CurUninstallStep = usUninstall then
  begin
    StopResident;
    if HistoryMetadataPresent then
    begin
      if SuppressibleMsgBox(
        'Remove Matteshot History metadata?'#13#10#13#10
        + 'This deletes stored window titles from AppData. Screenshot and video files stay on disk.',
        mbConfirmation, MB_YESNO, IDNO) = IDYES then
      begin
        if not DeleteHistoryMetadata then
          SuppressibleMsgBox(
            'Matteshot could not remove History metadata. You can delete history.json from AppData\Roaming\matteshot yourself.',
            mbError, MB_OK, IDOK);
      end;
    end;
  end;
end;
