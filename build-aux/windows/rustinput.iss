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
; asks for the language only when Windows speaks none of them
ShowLanguageDialog=auto

[Languages]
; the languages of RustInput; each one that this Inno Setup has (all of them
; since 6.5), the language of Windows picks it
Name: "english"; MessagesFile: "compiler:Default.isl"
Name: "brazilianportuguese"; MessagesFile: "compiler:Languages\BrazilianPortuguese.isl"
#if FileExists(CompilerPath + "Languages\Portuguese.isl")
Name: "portuguese"; MessagesFile: "compiler:Languages\Portuguese.isl"
#endif
#if FileExists(CompilerPath + "Languages\Spanish.isl")
Name: "spanish"; MessagesFile: "compiler:Languages\Spanish.isl"
#endif
#if FileExists(CompilerPath + "Languages\French.isl")
Name: "french"; MessagesFile: "compiler:Languages\French.isl"
#endif
#if FileExists(CompilerPath + "Languages\Italian.isl")
Name: "italian"; MessagesFile: "compiler:Languages\Italian.isl"
#endif
#if FileExists(CompilerPath + "Languages\Russian.isl")
Name: "russian"; MessagesFile: "compiler:Languages\Russian.isl"
#endif
#if FileExists(CompilerPath + "Languages\Arabic.isl")
Name: "arabic"; MessagesFile: "compiler:Languages\Arabic.isl"
#endif
#if FileExists(CompilerPath + "Languages\ChineseSimplified.isl")
Name: "chinesesimplified"; MessagesFile: "compiler:Languages\ChineseSimplified.isl"
#endif
#if FileExists(CompilerPath + "Languages\Japanese.isl")
Name: "japanese"; MessagesFile: "compiler:Languages\Japanese.isl"
#endif

[CustomMessages]
english.TaskService=Run RustInput as a Windows service (recommended): it starts with Windows, also without its window, and reaches programs run as administrator too
english.TaskTray=Show the RustInput icon in the tray when signing in to Windows
english.UpdateShortcut=Update RustInput
english.SettingUp=Setting up the Windows service and the firewall...
english.SetupFailed=RustInput is installed, but setting up the Windows service or the firewall failed (code %1).%n%nDetails: %2
english.ChangesTitle=Changes to Windows:
english.ChangeService=the Windows service "RustInput", which starts with Windows and runs RustInput with the administrator rights of the user at the screen
english.ChangeNoService=no Windows service: the window of RustInput starts it, without administrator rights (removes the service if there is one)
english.ChangeFirewall=the rules "RustInput (UDP)" and "RustInput (TCP)" of the Windows firewall, which let the other computers reach rustinput.exe
english.ChangeMenu=the item "Copy to RustInput" in the menu of Explorer, which RustInput adds (it can be turned off in its settings)
english.ChangeUninstall=Uninstalling (Settings > Apps) undoes all of it and keeps the settings of RustInput.

brazilianportuguese.TaskService=Rodar o RustInput como serviço do Windows (recomendado): inicia com o Windows, mesmo sem a janela, e alcança também os programas abertos como administrador
brazilianportuguese.TaskTray=Mostrar o ícone do RustInput na bandeja ao entrar no Windows
brazilianportuguese.UpdateShortcut=Atualizar o RustInput
brazilianportuguese.SettingUp=Configurando o serviço do Windows e o firewall...
brazilianportuguese.SetupFailed=O RustInput foi instalado, mas a configuração do serviço do Windows ou do firewall falhou (código %1).%n%nDetalhes: %2
brazilianportuguese.ChangesTitle=Mudanças no Windows:
brazilianportuguese.ChangeService=o serviço do Windows "RustInput", que inicia com o Windows e roda o RustInput com os direitos de administrador de quem está na tela
brazilianportuguese.ChangeNoService=nenhum serviço do Windows: a janela do RustInput o inicia, sem direitos de administrador (remove o serviço, se houver)
brazilianportuguese.ChangeFirewall=as regras "RustInput (UDP)" e "RustInput (TCP)" do firewall do Windows, que deixam os outros computadores alcançar o rustinput.exe
brazilianportuguese.ChangeMenu=o item "Copiar para o RustInput" no menu do Explorer, que o RustInput acrescenta (dá para desligar nas configurações)
brazilianportuguese.ChangeUninstall=Desinstalar (Configurações > Aplicativos) desfaz tudo isso e mantém as configurações do RustInput.

#if FileExists(CompilerPath + "Languages\Portuguese.isl")
portuguese.TaskService=Executar o RustInput como serviço do Windows (recomendado): inicia com o Windows, mesmo sem a janela, e chega também aos programas executados como administrador
portuguese.TaskTray=Mostrar o ícone do RustInput na área de notificação ao iniciar sessão no Windows
portuguese.UpdateShortcut=Atualizar o RustInput
portuguese.SettingUp=A configurar o serviço do Windows e a firewall...
portuguese.SetupFailed=O RustInput foi instalado, mas a configuração do serviço do Windows ou da firewall falhou (código %1).%n%nDetalhes: %2
portuguese.ChangesTitle=Alterações no Windows:
portuguese.ChangeService=o serviço do Windows "RustInput", que inicia com o Windows e executa o RustInput com os direitos de administrador do utilizador que está ao ecrã
portuguese.ChangeNoService=nenhum serviço do Windows: a janela do RustInput inicia-o, sem direitos de administrador (remove o serviço, se existir)
portuguese.ChangeFirewall=as regras "RustInput (UDP)" e "RustInput (TCP)" da firewall do Windows, que permitem aos outros computadores chegar ao rustinput.exe
portuguese.ChangeMenu=o item "Copiar para o RustInput" no menu do Explorador, que o RustInput acrescenta (pode ser desligado nas definições)
portuguese.ChangeUninstall=Desinstalar (Definições > Aplicações) desfaz tudo isto e mantém as definições do RustInput.
#endif

#if FileExists(CompilerPath + "Languages\Spanish.isl")
spanish.TaskService=Ejecutar RustInput como servicio de Windows (recomendado): se inicia con Windows, también sin su ventana, y llega también a los programas ejecutados como administrador
spanish.TaskTray=Mostrar el icono de RustInput en la bandeja al iniciar sesión en Windows
spanish.UpdateShortcut=Actualizar RustInput
spanish.SettingUp=Configurando el servicio de Windows y el firewall...
spanish.SetupFailed=RustInput está instalado, pero falló la configuración del servicio de Windows o del firewall (código %1).%n%nDetalles: %2
spanish.ChangesTitle=Cambios en Windows:
spanish.ChangeService=el servicio de Windows "RustInput", que se inicia con Windows y ejecuta RustInput con los derechos de administrador del usuario que está frente a la pantalla
spanish.ChangeNoService=ningún servicio de Windows: la ventana de RustInput lo inicia, sin derechos de administrador (elimina el servicio si existe)
spanish.ChangeFirewall=las reglas "RustInput (UDP)" y "RustInput (TCP)" del firewall de Windows, que permiten a los otros equipos llegar a rustinput.exe
spanish.ChangeMenu=el elemento "Copiar a RustInput" en el menú del Explorador, que agrega RustInput (se puede desactivar en sus preferencias)
spanish.ChangeUninstall=Desinstalar (Configuración > Aplicaciones) deshace todo esto y conserva las preferencias de RustInput.
#endif

#if FileExists(CompilerPath + "Languages\French.isl")
french.TaskService=Exécuter RustInput comme service Windows (recommandé) : il démarre avec Windows, même sans sa fenêtre, et atteint aussi les programmes lancés en tant qu’administrateur
french.TaskTray=Afficher l’icône de RustInput dans la zone de notification à l’ouverture de session
french.UpdateShortcut=Mettre à jour RustInput
french.SettingUp=Configuration du service Windows et du pare-feu...
french.SetupFailed=RustInput est installé, mais la configuration du service Windows ou du pare-feu a échoué (code %1).%n%nDétails : %2
french.ChangesTitle=Modifications de Windows :
french.ChangeService=le service Windows « RustInput », qui démarre avec Windows et exécute RustInput avec les droits d’administrateur de l’utilisateur devant l’écran
french.ChangeNoService=aucun service Windows : la fenêtre de RustInput le démarre, sans droits d’administrateur (supprime le service s’il existe)
french.ChangeFirewall=les règles « RustInput (UDP) » et « RustInput (TCP) » du pare-feu Windows, qui permettent aux autres ordinateurs d’atteindre rustinput.exe
french.ChangeMenu=l’élément « Copier vers RustInput » dans le menu de l’Explorateur, ajouté par RustInput (désactivable dans ses préférences)
french.ChangeUninstall=La désinstallation (Paramètres > Applications) annule tout cela et conserve les préférences de RustInput.
#endif

#if FileExists(CompilerPath + "Languages\Italian.isl")
italian.TaskService=Eseguire RustInput come servizio di Windows (consigliato): si avvia con Windows, anche senza la sua finestra, e raggiunge anche i programmi eseguiti come amministratore
italian.TaskTray=Mostrare l’icona di RustInput nell’area di notifica all’accesso a Windows
italian.UpdateShortcut=Aggiorna RustInput
italian.SettingUp=Configurazione del servizio di Windows e del firewall...
italian.SetupFailed=RustInput è installato, ma la configurazione del servizio di Windows o del firewall non è riuscita (codice %1).%n%nDettagli: %2
italian.ChangesTitle=Modifiche a Windows:
italian.ChangeService=il servizio di Windows "RustInput", che si avvia con Windows ed esegue RustInput con i diritti di amministratore dell’utente davanti allo schermo
italian.ChangeNoService=nessun servizio di Windows: la finestra di RustInput lo avvia, senza diritti di amministratore (rimuove il servizio se esiste)
italian.ChangeFirewall=le regole "RustInput (UDP)" e "RustInput (TCP)" del firewall di Windows, che permettono agli altri computer di raggiungere rustinput.exe
italian.ChangeMenu=la voce "Copia in RustInput" nel menu di Esplora file, aggiunta da RustInput (si può disattivare nelle sue preferenze)
italian.ChangeUninstall=La disinstallazione (Impostazioni > App) annulla tutto questo e mantiene le preferenze di RustInput.
#endif

#if FileExists(CompilerPath + "Languages\Russian.isl")
russian.TaskService=Запускать RustInput как службу Windows (рекомендуется): запускается вместе с Windows, даже без окна, и работает и с программами, запущенными от имени администратора
russian.TaskTray=Показывать значок RustInput в области уведомлений при входе в Windows
russian.UpdateShortcut=Обновить RustInput
russian.SettingUp=Настройка службы Windows и брандмауэра...
russian.SetupFailed=RustInput установлен, но настроить службу Windows или брандмауэр не удалось (код %1).%n%nПодробности: %2
russian.ChangesTitle=Изменения в Windows:
russian.ChangeService=служба Windows «RustInput», которая запускается вместе с Windows и выполняет RustInput с правами администратора пользователя за экраном
russian.ChangeNoService=без службы Windows: RustInput запускается из своего окна без прав администратора (служба удаляется, если она есть)
russian.ChangeFirewall=правила «RustInput (UDP)» и «RustInput (TCP)» брандмауэра Windows, которые позволяют другим компьютерам связываться с rustinput.exe
russian.ChangeMenu=пункт «Копировать в RustInput» в меню Проводника, который добавляет RustInput (его можно отключить в параметрах)
russian.ChangeUninstall=Удаление (Параметры > Приложения) отменяет всё это и сохраняет настройки RustInput.
#endif

#if FileExists(CompilerPath + "Languages\Arabic.isl")
arabic.TaskService=تشغيل RustInput كخدمة Windows (موصى به): يبدأ مع Windows حتى دون نافذته، ويصل أيضًا إلى البرامج التي تعمل كمسؤول
arabic.TaskTray=إظهار أيقونة RustInput في منطقة الإعلامات عند تسجيل الدخول إلى Windows
arabic.UpdateShortcut=تحديث RustInput
arabic.SettingUp=جارٍ إعداد خدمة Windows والجدار الناري...
arabic.SetupFailed=تم تثبيت RustInput، لكن فشل إعداد خدمة Windows أو الجدار الناري (الرمز %1).%n%nالتفاصيل: %2
arabic.ChangesTitle=التغييرات في Windows:
arabic.ChangeService=خدمة Windows ‏«RustInput»، التي تبدأ مع Windows وتشغّل RustInput بصلاحيات المسؤول للمستخدم الموجود أمام الشاشة
arabic.ChangeNoService=بدون خدمة Windows: تبدأ نافذة RustInput تشغيله دون صلاحيات المسؤول (وتُزال الخدمة إن وُجدت)
arabic.ChangeFirewall=القاعدتان «RustInput (UDP)» و«RustInput (TCP)» في جدار حماية Windows، اللتان تسمحان لأجهزة الكمبيوتر الأخرى بالوصول إلى rustinput.exe
arabic.ChangeMenu=العنصر «نسخ إلى RustInput» في قائمة المستكشف، الذي يضيفه RustInput (يمكن إيقافه من تفضيلاته)
arabic.ChangeUninstall=إلغاء التثبيت (الإعدادات > التطبيقات) يتراجع عن كل ذلك ويحتفظ بتفضيلات RustInput.
#endif

#if FileExists(CompilerPath + "Languages\ChineseSimplified.isl")
chinesesimplified.TaskService=将 RustInput 作为 Windows 服务运行（推荐）：随 Windows 启动，不打开窗口也能运行，并且也能控制以管理员身份运行的程序
chinesesimplified.TaskTray=登录 Windows 时在通知区域显示 RustInput 图标
chinesesimplified.UpdateShortcut=更新 RustInput
chinesesimplified.SettingUp=正在设置 Windows 服务和防火墙...
chinesesimplified.SetupFailed=RustInput 已安装，但设置 Windows 服务或防火墙失败（代码 %1）。%n%n详细信息：%2
chinesesimplified.ChangesTitle=对 Windows 的更改：
chinesesimplified.ChangeService=Windows 服务“RustInput”：随 Windows 启动，并以屏幕前用户的管理员权限运行 RustInput
chinesesimplified.ChangeNoService=不使用 Windows 服务：由 RustInput 窗口启动，不具有管理员权限（如已有该服务则将其删除）
chinesesimplified.ChangeFirewall=Windows 防火墙规则“RustInput (UDP)”和“RustInput (TCP)”：允许其他计算机连接到 rustinput.exe
chinesesimplified.ChangeMenu=资源管理器菜单中的“复制到 RustInput”项，由 RustInput 添加（可在其首选项中关闭）
chinesesimplified.ChangeUninstall=卸载（设置 > 应用）会撤销以上所有更改，并保留 RustInput 的设置。
#endif

#if FileExists(CompilerPath + "Languages\Japanese.isl")
japanese.TaskService=RustInput を Windows サービスとして実行する（推奨）: Windows と同時に起動し、ウィンドウを開かなくても動作し、管理者として実行されたプログラムも操作できます
japanese.TaskTray=Windows へのサインイン時に通知領域に RustInput のアイコンを表示する
japanese.UpdateShortcut=RustInput を更新
japanese.SettingUp=Windows サービスとファイアウォールを設定しています...
japanese.SetupFailed=RustInput はインストールされましたが、Windows サービスまたはファイアウォールの設定に失敗しました（コード %1）。%n%n詳細: %2
japanese.ChangesTitle=Windows への変更:
japanese.ChangeService=Windows サービス「RustInput」: Windows と同時に起動し、画面の前にいるユーザーの管理者権限で RustInput を実行します
japanese.ChangeNoService=Windows サービスなし: RustInput のウィンドウから管理者権限なしで起動します（サービスがある場合は削除します）
japanese.ChangeFirewall=Windows ファイアウォールの規則「RustInput (UDP)」と「RustInput (TCP)」: ほかのコンピューターから rustinput.exe に接続できるようにします
japanese.ChangeMenu=エクスプローラーのメニューの「RustInput にコピー」項目: RustInput が追加します（設定でオフにできます）
japanese.ChangeUninstall=アンインストール（設定 > アプリ）でこれらはすべて元に戻り、RustInput の設定は保持されます。
#endif

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
