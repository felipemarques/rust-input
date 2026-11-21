; The installer of RustInput for Windows (rustinput-windows-x86_64-setup.exe),
; built with Inno Setup 6 by .github/workflows/release.yml, and by
; .github/workflows/windows-service.yml for its tests:
;
;   ISCC.exe /DAppVersion=0.24.0 /DPackageDir=C:\path\rustinput-windows
;            /DOutputDir=C:\path\out build-aux\windows\rustinput.iss
;
; PackageDir holds what the zip of a release holds: rustinput.exe, the DLLs
; of GTK and update-rustinput.ps1. RustInput itself sets up the Windows
; service and the rules of the firewall (rustinput.exe windows-setup, see
; src/windows_setup.rs), which log into %SystemRoot%\Logs\RustInput\setup.log.
;
; It takes over an installation update-rustinput.ps1 made (the same folder,
; the same service, the same rules of the firewall), and update-rustinput.ps1
; runs it to install a version that has it; that script also goes back to a
; version without it (-Version 0.23.1), removing this installation first.

#ifndef AppVersion
  #error Give the version: /DAppVersion=x.y.z
#endif
#ifndef PackageDir
  #error Give the folder of the files: /DPackageDir=...
#endif
#ifndef OutputDir
  #define OutputDir "."
#endif

[Setup]
; never change: Windows and update-rustinput.ps1 know the installation by it
; (HKLM\...\Uninstall\{8C7A4E2B-3F1D-4B6A-9E5C-2D7F1A0B6C34}_is1)
AppId={{8C7A4E2B-3F1D-4B6A-9E5C-2D7F1A0B6C34}
AppName=RustInput
AppVersion={#AppVersion}
AppVerName=RustInput {#AppVersion}
AppPublisher=RustInput
AppPublisherURL=https://github.com/felipemarques/rust-input
AppSupportURL=https://github.com/felipemarques/rust-input/issues
AppUpdatesURL=https://github.com/felipemarques/rust-input/releases
AppCopyright=GPL-3.0-or-later. Based on lan-mouse by Ferdinand Schober and contributors
VersionInfoVersion={#AppVersion}.0
VersionInfoProductName=RustInput
VersionInfoProductVersion={#AppVersion}
VersionInfoProductTextVersion={#AppVersion}
VersionInfoTextVersion={#AppVersion}
VersionInfoDescription=RustInput Setup
VersionInfoCompany=RustInput
VersionInfoCopyright=GPL-3.0-or-later. Based on lan-mouse by Ferdinand Schober and contributors
; where update-rustinput.ps1 installs since 0.23.0
DefaultDirName={autopf}\RustInput
UsePreviousAppDir=yes
DisableDirPage=auto
DisableProgramGroupPage=yes
DisableWelcomePage=no
; the service and the firewall
PrivilegesRequired=admin
ArchitecturesAllowed=x64compatible
ArchitecturesInstallIn64BitMode=x64compatible
MinVersion=10.0
WizardStyle=modern
SetupIconFile=rustinput.ico
UninstallDisplayIcon={app}\rustinput.exe
UninstallDisplayName=RustInput
OutputDir={#OutputDir}
OutputBaseFilename=rustinput-windows-x86_64-setup
Compression=lzma2/max
SolidCompression=yes
; PrepareToInstall closes RustInput; this is for anything else holding a file
CloseApplications=yes
RestartApplications=no
SetupLogging=yes

[Languages]
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "brazilianportuguese"; MessagesFile: "compiler:Languages\BrazilianPortuguese.isl"

[CustomMessages]
english.TaskService=Run RustInput as a Windows service (recommended): it starts with Windows, also without its window, and reaches programs run as administrator too
brazilianportuguese.TaskService=Rodar o RustInput como serviço do Windows (recomendado): inicia com o Windows, mesmo sem a janela, e alcança também os programas abertos como administrador
english.TaskTray=Show the RustInput icon in the tray when signing in to Windows
brazilianportuguese.TaskTray=Mostrar o ícone do RustInput na bandeja ao entrar no Windows
english.UpdateShortcut=Update RustInput
brazilianportuguese.UpdateShortcut=Atualizar o RustInput
english.SettingUp=Setting up the Windows service and the firewall...
brazilianportuguese.SettingUp=Configurando o serviço do Windows e o firewall...
english.SetupFailed=RustInput is installed, but setting up the Windows service or the firewall failed (code %1).%n%nDetails: %2
brazilianportuguese.SetupFailed=O RustInput foi instalado, mas a configuração do serviço do Windows ou do firewall falhou (código %1).%n%nDetalhes: %2
english.ChangesTitle=Changes to Windows:
brazilianportuguese.ChangesTitle=Mudanças no Windows:
english.ChangeService=the Windows service "RustInput", which starts with Windows and runs RustInput with the administrator rights of the user at the screen
brazilianportuguese.ChangeService=o serviço do Windows "RustInput", que inicia com o Windows e roda o RustInput com os direitos de administrador de quem está na tela
english.ChangeNoService=no Windows service: the window of RustInput starts it, without administrator rights (removes the service if there is one)
brazilianportuguese.ChangeNoService=nenhum serviço do Windows: a janela do RustInput o inicia, sem direitos de administrador (remove o serviço, se houver)
english.ChangeFirewall=the rules "RustInput (UDP)" and "RustInput (TCP)" of the Windows firewall, which let the other computers reach rustinput.exe
brazilianportuguese.ChangeFirewall=as regras "RustInput (UDP)" e "RustInput (TCP)" do firewall do Windows, que deixam os outros computadores alcançar o rustinput.exe
english.ChangeMenu=the item "Copy to RustInput" in the menu of Explorer, which RustInput adds (it can be turned off in its settings)
brazilianportuguese.ChangeMenu=o item "Copiar para o RustInput" no menu do Explorer, que o RustInput acrescenta (dá para desligar nas configurações)
english.ChangeUninstall=Uninstalling (Settings > Apps) undoes all of it and keeps the settings of RustInput.
brazilianportuguese.ChangeUninstall=Desinstalar (Configurações > Aplicativos) desfaz tudo isso e mantém as configurações do RustInput.

[Tasks]
Name: "service"; Description: "{cm:TaskService}"
Name: "tray"; Description: "{cm:TaskTray}"
Name: "desktopicon"; Description: "{cm:CreateDesktopIcon}"; GroupDescription: "{cm:AdditionalIcons}"; Flags: unchecked

[InstallDelete]
; the DLLs of the version before: a newer GTK may not bring all of them
Type: files; Name: "{app}\*.dll"
; the Start menu items update-rustinput.ps1 made for the user
Type: files; Name: "{userprograms}\RustInput.lnk"
Type: files; Name: "{userprograms}\Update RustInput.lnk"
Type: files; Name: "{userprograms}\Atualizar o RustInput.lnk"
; what an interrupted run of update-rustinput.ps1 left beside the folder
Type: filesandordirs; Name: "{app}.new"
Type: filesandordirs; Name: "{app}.swap"

[UninstallDelete]
; also when the folder was there before the installer (update-rustinput.ps1
; made it): the uninstaller only removes the folders it made itself
Type: dirifempty; Name: "{app}"

[Files]
Source: "{#PackageDir}\*"; DestDir: "{app}"; Flags: ignoreversion recursesubdirs createallsubdirs

[Icons]
Name: "{autoprograms}\RustInput"; Filename: "{app}\rustinput.exe"; Comment: "RustInput"
Name: "{autoprograms}\{cm:UpdateShortcut}"; Filename: "{sys}\WindowsPowerShell\v1.0\powershell.exe"; Parameters: "-NoProfile -ExecutionPolicy Bypass -File ""{app}\update-rustinput.ps1"""; IconFilename: "{app}\rustinput.exe"; Comment: "{cm:UpdateShortcut}"
Name: "{autodesktop}\RustInput"; Filename: "{app}\rustinput.exe"; Tasks: desktopicon
Name: "{commonstartup}\RustInput"; Filename: "{app}\rustinput.exe"; Tasks: tray

[Run]
; without administrator rights, like the Start menu would
Filename: "{app}\rustinput.exe"; Description: "{cm:LaunchProgram,RustInput}"; Flags: nowait postinstall skipifsilent runasoriginaluser

[Code]
const
  MenuKey = 'Software\Classes\AllFilesystemObjects\shell\RustInput.CopyTo';

var
  SetupFailure: Integer;
  UninstallDone: Boolean;

function SetupLog: String;
begin
  Result := ExpandConstant('{win}\Logs\RustInput\setup.log');
end;

function RunHidden(const Filename, Arguments: String): Integer;
var
  ResultCode: Integer;
begin
  if Exec(Filename, Arguments, '', SW_HIDE, ewWaitUntilTerminated, ResultCode) then
    Result := ResultCode
  else
    Result := -1;
end;

// The service, the daemon it started (which ends with it), the windows of
// every user and "Copy to RustInput": their files are about to be replaced.
// net.exe waits until the service stopped.
procedure CloseRustInput;
begin
  RunHidden(ExpandConstant('{sys}\net.exe'), 'stop RustInput');
  RunHidden(ExpandConstant('{sys}\taskkill.exe'), '/F /IM rustinput.exe');
  Sleep(500);
end;

function PrepareToInstall(var NeedsRestart: Boolean): String;
begin
  CloseRustInput;
  Result := '';
end;

procedure CurStepChanged(CurStep: TSetupStep);
var
  Arguments: String;
begin
  if CurStep = ssPostInstall then
  begin
    if not WizardSilent then
      WizardForm.StatusLabel.Caption := CustomMessage('SettingUp');
    Arguments := 'windows-setup install';
    if not WizardIsTaskSelected('service') then
      Arguments := Arguments + ' --no-service';
    SetupFailure := RunHidden(ExpandConstant('{app}\rustinput.exe'), Arguments);
    Log(Format('rustinput.exe %s: %d', [Arguments, SetupFailure]));
    if SetupFailure <> 0 then
      SuppressibleMsgBox(FmtMessage(CustomMessage('SetupFailed'), [IntToStr(SetupFailure), SetupLog]), mbError, MB_OK, IDOK);
  end;
end;

// 0, or 100 plus the code of rustinput.exe windows-setup, 199 when it did
// not start at all (Inno Setup itself ends with 1 to 8)
function GetCustomSetupExitCode: Integer;
begin
  if SetupFailure = 0 then
    Result := 0
  else if SetupFailure < 0 then
    Result := 199
  else
    Result := 100 + SetupFailure;
end;

function UpdateReadyMemo(Space, NewLine, MemoUserInfoInfo, MemoDirInfo, MemoTypeInfo, MemoComponentsInfo, MemoGroupInfo, MemoTasksInfo: String): String;
begin
  Result := MemoDirInfo + NewLine + NewLine;
  if MemoTasksInfo <> '' then
    Result := Result + MemoTasksInfo + NewLine + NewLine;
  Result := Result + CustomMessage('ChangesTitle') + NewLine;
  if WizardIsTaskSelected('service') then
    Result := Result + Space + CustomMessage('ChangeService') + NewLine
  else
    Result := Result + Space + CustomMessage('ChangeNoService') + NewLine;
  Result := Result + Space + CustomMessage('ChangeFirewall') + NewLine;
  Result := Result + Space + CustomMessage('ChangeMenu') + NewLine + NewLine;
  Result := Result + CustomMessage('ChangeUninstall');
end;

procedure CurUninstallStepChanged(CurUninstallStep: TUninstallStep);
var
  Code: Integer;
begin
  if CurUninstallStep = usUninstall then
  begin
    // the service and the rules of the firewall, while rustinput.exe is there
    Code := RunHidden(ExpandConstant('{app}\rustinput.exe'), 'windows-setup uninstall');
    Log(Format('rustinput.exe windows-setup uninstall: %d', [Code]));
    UninstallDone := Code = 0;
    if not UninstallDone then
    begin
      RunHidden(ExpandConstant('{sys}\net.exe'), 'stop RustInput');
      RunHidden(ExpandConstant('{sys}\sc.exe'), 'delete RustInput');
      RunHidden(ExpandConstant('{sys}\netsh.exe'), 'advfirewall firewall delete rule name="RustInput (UDP)"');
      RunHidden(ExpandConstant('{sys}\netsh.exe'), 'advfirewall firewall delete rule name="RustInput (TCP)"');
    end;
    CloseRustInput;
    // added by RustInput for the user who uninstalls
    RegDeleteKeyIncludingSubkeys(HKCU, MenuKey);
  end;
  if CurUninstallStep = usPostUninstall then
  begin
    // the logs of the service and of this setup stay when something failed
    if UninstallDone then
      DelTree(ExpandConstant('{win}\Logs\RustInput'), True, True, True);
    // the copy update-rustinput.ps1 kept of the version before
    DelTree(ExpandConstant('{app}') + '.previous', True, True, True);
  end;
end;
