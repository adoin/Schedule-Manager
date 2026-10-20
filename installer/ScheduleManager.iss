#define AppName "Schedule Manager"
#ifndef AppVersion
#define AppVersion "1.0.3"
#endif
#ifndef SourceDir
#define SourceDir "..\dist\ScheduleManager"
#endif
#ifndef OutputDir
#define OutputDir "..\dist\installer"
#endif
#define AppPublisher "Emssion"
#define AppExeName "ScheduleManager.exe"

[Setup]
AppId={{8021ACD7-6E49-4FD8-87DF-2D663D29A930}
AppName={#AppName}
AppVersion={#AppVersion}
AppPublisher={#AppPublisher}
DefaultDirName={autopf}\{#AppName}
DefaultGroupName={#AppName}
OutputDir={#OutputDir}
OutputBaseFilename=ScheduleManager-Setup-{#AppVersion}
Compression=lzma2
SolidCompression=yes
WizardStyle=modern
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
UninstallDisplayIcon={app}\{#AppExeName}
; WM_CLOSE may hide the manager in the tray, and the widget is a WorkerW child.
; Use the bounded, installation-scoped shutdown helper instead of Restart Manager.
CloseApplications=no
RestartApplications=no

[Messages]
ReadyLabel2a=Setup will close Schedule Manager and its desktop widget. Save any pending edits before clicking Install.
ReadyLabel2b=Setup will close Schedule Manager and its desktop widget. Save any pending edits before clicking Install.

[Files]
Source: "{#SourceDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\{#AppName}"; Filename: "{app}\{#AppExeName}"
Name: "{autodesktop}\{#AppName}"; Filename: "{app}\{#AppExeName}"; Tasks: desktopicon

[Run]
Filename: "{app}\{#AppExeName}"; Description: "Launch {#AppName}"; Flags: nowait postinstall skipifsilent

[UninstallRun]
Filename: "{sys}\schtasks.exe"; Parameters: "/Delete /TN ""Schedule Manager"" /F"; Flags: runhidden waituntilterminated

[Tasks]
Name: "desktopicon"; Description: "Create a desktop shortcut"; GroupDescription: "Additional shortcuts:"

[Code]
function StopInstalledApplications(const Helper: String): String;
var
  ExitCode: Integer;
begin
  Result := '';
  if not Exec(Helper, '--shutdown-for-upgrade ' + AddQuotes(ExpandConstant('{app}')),
    '', SW_HIDE, ewWaitUntilTerminated, ExitCode) then
    Result := 'Unable to start application shutdown. Please fully exit Schedule Manager and retry.'
  else if ExitCode <> 0 then
    Result := 'Some Schedule Manager processes could not be closed. Please fully exit the main program and desktop widget, then retry.';
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  Result := '';
  if not DirExists(ExpandConstant('{app}')) then Exit;
  try
    ExtractTemporaryFile('{#AppExeName}');
    Result := StopInstalledApplications(ExpandConstant('{tmp}\{#AppExeName}'));
  except
    Result := 'Unable to prepare application shutdown: ' + GetExceptionMessage;
  end;
end;

function InitializeUninstall(): Boolean;
var
  Error: String;
begin
  Result := True;
  if not FileExists(ExpandConstant('{app}\{#AppExeName}')) then Exit;
  Error := StopInstalledApplications(ExpandConstant('{app}\{#AppExeName}'));
  if Error <> '' then begin
    MsgBox(Error, mbError, MB_OK);
    Result := False;
  end;
end;
