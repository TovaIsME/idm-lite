; IDM-lite setup wizard. Built by tools/package.py, which passes:
;   /DAppVersion=1.0.5          from Cargo.toml
;   /DPayload=<dist\IDM-lite>   the staged exe, updater and extension
;
; Installs for the current user only (no UAC) into
; %LOCALAPPDATA%\Programs\IDM-lite, the same folder earlier versions used, so
; it upgrades them in place.

#ifndef AppVersion
  #error Build this through tools/package.py
#endif

[Setup]
AppId=IDM-lite
AppName=IDM-lite
AppVersion={#AppVersion}
AppVerName=IDM-lite {#AppVersion}
AppPublisher=Tova
AppPublisherURL=https://github.com/tovaisme/idm-lite
AppSupportURL=https://github.com/tovaisme/idm-lite/issues
AppUpdatesURL=https://github.com/tovaisme/idm-lite/releases
VersionInfoVersion={#AppVersion}
DefaultDirName={autopf}\IDM-lite
DisableProgramGroupPage=yes
PrivilegesRequired=lowest
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
OutputBaseFilename=IDM-lite-Setup-{#AppVersion}
SetupIconFile=..\icon.ico
UninstallDisplayIcon={app}\IDM-lite.exe
UninstallDisplayName=IDM-lite
WizardStyle=modern
Compression=lzma2/max
SolidCompression=yes
; the tray app holds its exe open; close it rather than fail on the copy
CloseApplications=force
RestartApplications=no

[Tasks]
Name: desktopicon; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[InstallDelete]
; what the old self-made installer kept around so it could uninstall itself
Type: files; Name: "{app}\IDM-lite-Setup.exe"

[Files]
Source: "{#Payload}\IDM-lite.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Payload}\update.exe"; DestDir: "{app}"; Flags: ignoreversion
Source: "{#Payload}\extension\*"; DestDir: "{app}\extension"; Flags: ignoreversion recursesubdirs

[Icons]
Name: "{autoprograms}\IDM-lite"; Filename: "{app}\IDM-lite.exe"; Comment: "IDM-lite download manager"
Name: "{autodesktop}\IDM-lite"; Filename: "{app}\IDM-lite.exe"; Tasks: desktopicon

[Run]
; the app writes the native messaging manifest and the browser keys itself
Filename: "{app}\IDM-lite.exe"; Parameters: "--register"; StatusMsg: "Registering with Chrome, Edge, Brave and Chromium..."; Flags: runhidden waituntilterminated
Filename: "{app}\extension"; Description: "Open the extension folder (to load it in chrome://extensions)"; Flags: postinstall shellexec skipifsilent
Filename: "{app}\IDM-lite.exe"; Description: "{cm:LaunchProgram,IDM-lite}"; Flags: postinstall nowait skipifsilent

[UninstallRun]
Filename: "{app}\IDM-lite.exe"; Parameters: "--unregister"; Flags: runhidden waituntilterminated; RunOnceId: "Unregister"
Filename: "{sys}\taskkill.exe"; Parameters: "/IM IDM-lite.exe /F"; Flags: runhidden waituntilterminated; RunOnceId: "Stop"

[Code]
const
  WebView2Client = 'Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}';
  WebView2Setup = 'https://go.microsoft.com/fwlink/p/?LinkId=2124703';

var
  DownloadPage: TDownloadWizardPage;

function HasVersion(Root: Integer; Key: String): Boolean;
var
  V: String;
begin
  Result := RegQueryStringValue(Root, Key, 'pv', V) and (V <> '') and (V <> '0.0.0.0');
end;

{ The window is drawn by WebView2. Windows 11 always has it; a Windows 10
  machine that never got the new Edge may not, and then IDM-lite cannot open. }
function WebView2Installed: Boolean;
begin
  Result := HasVersion(HKLM, 'SOFTWARE\WOW6432Node\' + WebView2Client)
         or HasVersion(HKLM, 'SOFTWARE\' + WebView2Client)
         or HasVersion(HKCU, 'Software\' + WebView2Client);
end;

procedure InitializeWizard;
begin
  DownloadPage := CreateDownloadPage(SetupMessage(msgWizardPreparing),
    'Getting Microsoft Edge WebView2, which IDM-lite draws its window with.', nil);
end;

function NextButtonClick(CurPageID: Integer): Boolean;
var
  Code: Integer;
begin
  Result := True;
  if (CurPageID <> wpReady) or WebView2Installed then
    Exit;
  DownloadPage.Clear;
  DownloadPage.Add(WebView2Setup, 'MicrosoftEdgeWebview2Setup.exe', '');
  DownloadPage.Show;
  try
    try
      DownloadPage.Download;
      Exec(ExpandConstant('{tmp}\MicrosoftEdgeWebview2Setup.exe'), '/silent /install', '',
        SW_HIDE, ewWaitUntilTerminated, Code);
    except
      SuppressibleMsgBox('Could not install Microsoft Edge WebView2:' + #13#10 +
        GetExceptionMessage + #13#10#13#10 +
        'Setup will continue. If IDM-lite does not open afterwards, install WebView2 from' + #13#10 +
        'https://developer.microsoft.com/microsoft-edge/webview2/',
        mbError, MB_OK, IDOK);
    end;
  finally
    DownloadPage.Hide;
  end;
end;

procedure CurStepChanged(CurStep: TSetupStep);
begin
  { the old installer's Add/Remove Programs entry; this one is IDM-lite_is1 }
  if CurStep = ssPostInstall then
    RegDeleteKeyIncludingSubkeys(HKCU, 'Software\Microsoft\Windows\CurrentVersion\Uninstall\IDM-lite');
end;

procedure CurPageChanged(CurPageID: Integer);
begin
  if CurPageID = wpFinished then
    WizardForm.FinishedLabel.Caption :=
      'IDM-lite is installed and registered with your browsers.' + #13#10#13#10 +
      'One step is left, and Chrome only lets you do it yourself:' + #13#10#13#10 +
      '  1. open chrome://extensions' + #13#10 +
      '  2. turn on Developer mode (top right)' + #13#10 +
      '  3. click Load unpacked and choose:' + #13#10#13#10 +
      '      ' + ExpandConstant('{app}\extension');
end;
