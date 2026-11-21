<#
.SYNOPSIS
    Installs, updates or removes RustInput on Windows.

.DESCRIPTION
    Finds the newest release of RustInput on GitHub, downloads it with a
    progress bar, checks its SHA-256, closes RustInput, installs it while
    keeping a copy of the previous one, starts RustInput again and shows
    whether it came up, with the end of its logs.

    Since 0.24.0 a release brings an installer
    (rustinput-windows-x86_64-setup.exe), which this script runs without
    questions; RustInput then shows in Settings > Apps. Installing an older
    version (-Version 0.23.1) removes that installation first, keeping the
    settings, and installs the zip as before.

    RustInput is installed in C:\Program Files\RustInput with the Windows
    service "RustInput", which starts with Windows and keeps RustInput running
    in the session of the user at the screen with the administrator rights of
    that user: Windows keeps programs run as administrator out of reach of
    programs that run without them. That needs administrator rights once per
    run of this script, which asks Windows for them.

    Everything shown is also written to %LOCALAPPDATA%\rustinput\logs\update.log.
    Messages are in Portuguese when Windows is, in English otherwise.

.PARAMETER Version
    The version to install, like 0.20.1. The newest one by default. Also
    installs an older version, to go back to it.

.PARAMETER Force
    Installs even when that version is already installed.

.PARAMETER Restore
    Goes back to the copy the last update kept (running it again undoes it).

.PARAMETER Uninstall
    Removes RustInput: the service, the program, the Start menu items, the
    "Copy to RustInput" item of Explorer and the firewall rules. The settings
    and the logs stay. Same as uninstalling it in Settings > Apps.

.PARAMETER NoService
    Installs without the Windows service, as before 0.23.0: RustInput then runs
    without administrator rights and can not reach programs run as
    administrator. Removes the service if it is installed.

.PARAMETER NoStart
    Does not open the window of RustInput afterwards (the service starts
    anyway).

.PARAMETER NoShortcuts
    Does not create the "RustInput" and "Update RustInput" Start menu items
    (with the installer: no tray icon at sign-in and no desktop shortcut).

.PARAMETER NoPause
    Does not wait for Enter before the window closes.

.PARAMETER InstallDir
    Where RustInput is installed, C:\Program Files\RustInput by default.

.PARAMETER Zip
    Installs this rustinput-windows-x86_64.zip instead of downloading one.

.PARAMETER Setup
    Runs this rustinput-windows-x86_64-setup.exe instead of downloading one.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File update-rustinput.ps1

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File update-rustinput.ps1 -Version 0.18.0
#>
[CmdletBinding()]
param(
    [string]$Version = 'latest',
    [switch]$Force,
    [switch]$Restore,
    [switch]$Uninstall,
    [switch]$NoService,
    [switch]$NoStart,
    [switch]$NoShortcuts,
    [switch]$NoPause,
    [string]$InstallDir = '',
    [string]$Zip = '',
    [string]$Setup = ''
)

$ErrorActionPreference = 'Stop'
# the progress of the cmdlets slows Windows PowerShell down; the download
# shows its own
$ProgressPreference = 'SilentlyContinue'
# handed on when the script starts itself again as administrator
$ScriptParameters = $PSBoundParameters

$Repo = 'felipemarques/rust-input'
$AssetName = 'rustinput-windows-x86_64.zip'
$SetupAssetName = 'rustinput-windows-x86_64-setup.exe'
$ScriptName = 'update-rustinput.ps1'
$UserAgent = 'rustinput-updater'
$ServiceName = 'RustInput'
$FirewallGroup = 'RustInput'
# the first version with the service
$FirstServiceVersion = [version]'0.23.0'
# the installation by the installer, as Windows lists it in Settings > Apps
# (AppId of build-aux/windows/rustinput.iss)
$SetupKey = 'SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{8C7A4E2B-3F1D-4B6A-9E5C-2D7F1A0B6C34}_is1'
# 64-bit Program Files, also from a 32-bit PowerShell
$ProgramFilesDir = $env:ProgramW6432
if (-not $ProgramFilesDir) { $ProgramFilesDir = $env:ProgramFiles }
# where versions before 0.23.0 were installed
$OldDefaultDir = Join-Path $env:LOCALAPPDATA 'Programs\RustInput'
if (-not $InstallDir) {
    $InstallDir = Join-Path $ProgramFilesDir 'RustInput'
}
$InstallDir = $InstallDir.TrimEnd('\')
$Exe = Join-Path $InstallDir 'rustinput.exe'
$PreviousDir = "$InstallDir.previous"
$StagingDir = "$InstallDir.new"
$LogDir = Join-Path $env:LOCALAPPDATA 'rustinput\logs'
$UpdateLog = Join-Path $LogDir 'update.log'
$ServiceLogDir = Join-Path $env:SystemRoot 'Logs\RustInput'
$ServiceLog = Join-Path $ServiceLogDir 'service.log'
$MenuKey = 'HKCU:\Software\Classes\AllFilesystemObjects\shell\RustInput.CopyTo'

# this script, kept to be installed next to RustInput: when it runs from the
# installed copy, that copy is moved away before the end
$SelfBytes = $null
if ($PSCommandPath) {
    try { $SelfBytes = [System.IO.File]::ReadAllBytes($PSCommandPath) } catch { }
}

# started to run this script only ("Run with PowerShell", a shortcut, -File):
# the window closes at the end, so it waits for Enter first
$OwnWindow = @([Environment]::GetCommandLineArgs() | Where-Object { $_ -match '^-(File|Command|f|c)$' }).Count -gt 0

$Portuguese = (Get-UICulture).Name -like 'pt*'
function T([string]$English, [string]$Portugues) {
    if ($Portuguese) { return $Portugues }
    return $English
}

# ---------------------------------------------------------------------------
# output

$script:StepNumber = 0
$script:StepCount = 6

function Write-Step([string]$Text) {
    $script:StepNumber++
    Write-Host ''
    Write-Host ('[{0}/{1}] {2}' -f $script:StepNumber, $script:StepCount, $Text) -ForegroundColor Cyan
}

function Write-Info([string]$Text) {
    Write-Host "      $Text"
}

function Write-Detail([string]$Text) {
    Write-Host "      $Text" -ForegroundColor DarkGray
}

function Write-Ok([string]$Text) {
    Write-Host '  [ok] ' -ForegroundColor Green -NoNewline
    Write-Host $Text
}

function Write-Warn([string]$Text) {
    Write-Host '  [!]  ' -ForegroundColor Yellow -NoNewline
    Write-Host $Text -ForegroundColor Yellow
}

function Write-Fail([string]$Text) {
    Write-Host '  [x]  ' -ForegroundColor Red -NoNewline
    Write-Host $Text -ForegroundColor Red
}

function Write-Banner {
    Write-Host ''
    Write-Host '  RustInput  ' -ForegroundColor Black -BackgroundColor DarkYellow -NoNewline
    Write-Host (T '  install and update for Windows' '  instalação e atualização para Windows')
}

function Format-Size([double]$Bytes) {
    if ($Bytes -ge 1GB) { return '{0:N2} GB' -f ($Bytes / 1GB) }
    if ($Bytes -ge 1MB) { return '{0:N1} MB' -f ($Bytes / 1MB) }
    if ($Bytes -ge 1KB) { return '{0:N0} KB' -f ($Bytes / 1KB) }
    return '{0:N0} B' -f $Bytes
}

function Format-Date($Value) {
    try { return ([datetime]$Value).ToLocalTime().ToString('g') } catch { return "$Value" }
}

$script:TranscriptOn = $false

function Exit-Script([int]$Code) {
    if ($script:TranscriptOn) {
        try { Stop-Transcript | Out-Null } catch { }
    }
    if ($OwnWindow -and -not $NoPause) {
        Write-Host ''
        Read-Host (T 'Press Enter to close' 'Aperte Enter para fechar') | Out-Null
    }
    exit $Code
}

# ---------------------------------------------------------------------------
# administrator rights

function Test-Administrator {
    $principal = New-Object Security.Principal.WindowsPrincipal ([Security.Principal.WindowsIdentity]::GetCurrent())
    return $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)
}

function Test-UnderFolder([string]$Path, [string]$Folder) {
    return $Path.StartsWith($Folder.TrimEnd('\') + '\', [StringComparison]::OrdinalIgnoreCase)
}

# the service, and the folders of Program Files, are for administrators only
function Test-NeedsAdministrator {
    if (-not $NoService -and -not ($Uninstall -and -not (Test-Path -LiteralPath $Exe))) { return $true }
    if (Get-Service -Name $ServiceName -ErrorAction SilentlyContinue) { return $true }
    if ($Setup -or (Get-SetupInstall)) { return $true }
    return (Test-UnderFolder $InstallDir $ProgramFilesDir)
}

# Runs this script again as administrator, after Windows asks the user, and
# ends with its exit code. It shows its output in a window of its own.
function Invoke-Elevated {
    Write-Banner
    if (-not $PSCommandPath) {
        Write-Fail (T 'Save this script to a file and run that file: it needs to start itself again as administrator.' 'Salve este script num arquivo e rode o arquivo: ele precisa se abrir de novo como administrador.')
        exit 1
    }
    $arguments = @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', ('"{0}"' -f $PSCommandPath))
    foreach ($entry in $ScriptParameters.GetEnumerator()) {
        if ($entry.Value -is [System.Management.Automation.SwitchParameter]) {
            if ($entry.Value.IsPresent) { $arguments += ('-{0}' -f $entry.Key) }
        } else {
            $arguments += ('-{0}' -f $entry.Key)
            $arguments += ('"{0}"' -f $entry.Value)
        }
    }
    Write-Host ''
    Write-Info (T 'RustInput runs as a Windows service, which needs administrator rights:' 'O RustInput roda como serviço do Windows, o que precisa de direitos de administrador:')
    Write-Info (T 'confirm in the window Windows shows. The installation goes on in a window of its own.' 'confirme na janela que o Windows mostrar. A instalação continua numa janela própria.')
    $shell = (Get-Process -Id $PID).Path
    try {
        $process = Start-Process -FilePath $shell -ArgumentList $arguments -Verb RunAs -Wait -PassThru
    } catch {
        Write-Fail (T 'Windows did not give administrator rights.' 'O Windows não deu direitos de administrador.')
        Write-Detail ((T 'to install without the service, as before: add  -NoService -InstallDir "{0}"' 'para instalar sem o serviço, como antes: acrescente  -NoService -InstallDir "{0}"') -f $OldDefaultDir)
        exit 1
    }
    if ($process.ExitCode -eq 0) {
        Write-Ok (T 'done (the administrator window showed the details)' 'pronto (a janela de administrador mostrou os detalhes)')
    } else {
        Write-Fail ((T 'it did not work (code {0}), see {1}' 'não deu certo (código {0}), veja {1}') -f $process.ExitCode, $UpdateLog)
    }
    exit $process.ExitCode
}

# ---------------------------------------------------------------------------
# versions

function ConvertTo-Version([string]$Text) {
    $parsed = $null
    if ($Text -and [version]::TryParse(($Text.TrimStart('v') -replace '-.*$', ''), [ref]$parsed)) {
        return $parsed
    }
    return $null
}

# the version of an installed rustinput.exe: '' when it is older than 0.20.1,
# which has no version resource, $null when there is none
function Get-ExeVersion([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return $null }
    $version = (Get-Item -LiteralPath $Path).VersionInfo.ProductVersion
    if ($version) { return $version.Trim() }
    return ''
}

# whether that version of rustinput.exe can run as the service
function Test-ServiceVersion($Version) {
    $parsed = ConvertTo-Version $Version
    return ($parsed -and $parsed -ge $FirstServiceVersion)
}

# a token for the API of GitHub, when RUSTINPUT_GITHUB_TOKEN holds one: the
# tests on GitHub share addresses that run into the limit of requests
function Get-GitHubHeaders {
    $headers = @{}
    if ($env:RUSTINPUT_GITHUB_TOKEN) { $headers['Authorization'] = 'Bearer ' + $env:RUSTINPUT_GITHUB_TOKEN }
    return $headers
}

function Get-Release([string]$Wanted) {
    $api = "https://api.github.com/repos/$Repo/releases/"
    if ($Wanted -eq 'latest') {
        $url = $api + 'latest'
    } else {
        $url = $api + 'tags/v' + $Wanted.TrimStart('v')
    }
    try {
        return Invoke-RestMethod -Uri $url -UserAgent $UserAgent -Headers (Get-GitHubHeaders) -TimeoutSec 30
    } catch {
        $err = $_
        $status = $null
        try { $status = [int]$err.Exception.Response.StatusCode } catch { }
        if ($status -eq 404) {
            throw (T "there is no release $Wanted on GitHub (https://github.com/$Repo/releases)" "não existe a versão $Wanted no GitHub (https://github.com/$Repo/releases)")
        }
        if ($status -eq 403 -or $status -eq 429) {
            throw (T 'GitHub refused for now (too many requests from this network). Try again in an hour.' 'O GitHub recusou por enquanto (pedidos demais desta rede). Tente de novo daqui a uma hora.')
        }
        throw ((T 'could not reach GitHub: ' 'não deu para falar com o GitHub: ') + $err.Exception.Message)
    }
}

# the commits between two tags, newest first, without the version bumps
function Get-Changes([string]$From, [string]$To) {
    try {
        $compare = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/compare/$From...$To" -UserAgent $UserAgent -Headers (Get-GitHubHeaders) -TimeoutSec 30
    } catch {
        return @()
    }
    $subjects = @($compare.commits | ForEach-Object { ($_.commit.message -split "`n")[0].Trim() })
    [array]::Reverse($subjects)
    return @($subjects | Where-Object { $_ -and $_ -notmatch '^chore: bump version' })
}

# ---------------------------------------------------------------------------
# download

function Save-File([string]$Url, [string]$Path, [long]$ExpectedSize, [string]$Name) {
    # shown here only (see the top)
    $ProgressPreference = 'Continue'
    Add-Type -AssemblyName System.Net.Http
    $client = New-Object System.Net.Http.HttpClient
    $client.Timeout = [TimeSpan]::FromMinutes(30)
    $client.DefaultRequestHeaders.UserAgent.ParseAdd($UserAgent)
    $activity = (T 'Downloading ' 'Baixando ') + $Name
    $watch = [System.Diagnostics.Stopwatch]::StartNew()
    $done = [long]0
    try {
        $response = $client.GetAsync($Url, [System.Net.Http.HttpCompletionOption]::ResponseHeadersRead).GetAwaiter().GetResult()
        [void]$response.EnsureSuccessStatusCode()
        $total = $response.Content.Headers.ContentLength
        if (-not $total) { $total = $ExpectedSize }
        $source = $response.Content.ReadAsStreamAsync().GetAwaiter().GetResult()
        $target = [System.IO.File]::Create($Path)
        try {
            $buffer = New-Object byte[] 262144
            $shown = [long]-1000
            while (($read = $source.Read($buffer, 0, $buffer.Length)) -gt 0) {
                $target.Write($buffer, 0, $read)
                $done += $read
                if ($watch.ElapsedMilliseconds - $shown -ge 200) {
                    $shown = $watch.ElapsedMilliseconds
                    $speed = $done / [Math]::Max($watch.Elapsed.TotalSeconds, 0.001)
                    $status = '{0} / {1}   {2}/s' -f (Format-Size $done), (Format-Size $total), (Format-Size $speed)
                    if ($total -gt 0) {
                        $percent = [int][Math]::Min(100, 100 * $done / $total)
                        $left = [int](($total - $done) / [Math]::Max($speed, 1))
                        Write-Progress -Activity $activity -Status $status -PercentComplete $percent -SecondsRemaining $left
                    } else {
                        Write-Progress -Activity $activity -Status $status
                    }
                }
            }
        } finally {
            $target.Dispose()
            $source.Dispose()
        }
    } finally {
        $client.Dispose()
        Write-Progress -Activity $activity -Completed
    }
    return New-Object PSObject -Property @{ Bytes = $done; Seconds = $watch.Elapsed.TotalSeconds }
}

# unpacks a release next to the installed copy, to be put in its place
function Expand-Release([string]$ZipPath) {
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    Remove-Folder $StagingDir
    [System.IO.Compression.ZipFile]::ExtractToDirectory($ZipPath, $StagingDir)
    $stagedExe = Join-Path $StagingDir 'rustinput.exe'
    if (-not (Test-Path -LiteralPath $stagedExe)) {
        Remove-Folder $StagingDir
        throw ((T '{0} has no rustinput.exe' 'o {0} não tem o rustinput.exe') -f (Split-Path -Leaf $ZipPath))
    }
    $files = @(Get-ChildItem -LiteralPath $StagingDir -Recurse -File)
    $size = ($files | Measure-Object -Property Length -Sum).Sum
    Write-Ok ((T 'unpacked: {0} files, {1}' 'descompactado: {0} arquivos, {1}') -f $files.Count, (Format-Size $size))
    return (Get-ExeVersion $stagedExe)
}

# ---------------------------------------------------------------------------
# processes

# The rustinput.exe running: the service, the daemon the service or the
# window started, the window, "Copy to RustInput".
function Get-RustInputProcess {
    $found = @()
    foreach ($process in @(Get-CimInstance -ClassName Win32_Process -Filter "Name='rustinput.exe'" -ErrorAction SilentlyContinue)) {
        $line = [string]$process.CommandLine
        if ($line -match '\swindows-service\s*$') {
            $role = 'service'
        } elseif ($line -match '\sdaemon(\s|$)') {
            $role = 'daemon'
        } elseif ($line -match '\ssend-files\s') {
            $role = 'send-files'
        } else {
            $role = 'window'
        }
        $found += New-Object PSObject -Property @{
            Id = [int]$process.ProcessId
            Path = [string]$process.ExecutablePath
            Role = $role
            Session = [int]$process.SessionId
        }
    }
    return $found
}

function Get-RoleText([string]$Role) {
    switch ($Role) {
        'service' { return (T 'service' 'serviço') }
        'daemon' { return 'daemon' }
        'window' { return (T 'window' 'janela') }
        default { return $Role }
    }
}

function Stop-RustInputService {
    $service = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
    if (-not $service -or $service.Status -eq 'Stopped') { return }
    try {
        Stop-Service -Name $ServiceName -Force -ErrorAction Stop -WarningAction SilentlyContinue
        $service.WaitForStatus('Stopped', [TimeSpan]::FromSeconds(20))
        Write-Ok (T 'service stopped' 'serviço parado')
    } catch {
        Write-Warn ((T 'the service did not stop: {0}' 'o serviço não parou: {0}') -f $_.Exception.Message)
    }
}

function Stop-RustInput {
    Stop-RustInputService
    $running = @(Get-RustInputProcess)
    if ($running.Count -eq 0) {
        Write-Info (T 'RustInput is not running.' 'O RustInput não está aberto.')
        return
    }
    foreach ($process in $running) {
        $path = $process.Path
        if (-not $path) { $path = '?' }
        Write-Detail ('PID {0}  {1}  {2}' -f $process.Id, (Get-RoleText $process.Role), $path)
    }
    $ids = @($running | ForEach-Object { $_.Id })
    Stop-Process -Id $ids -Force -ErrorAction SilentlyContinue
    Wait-Process -Id $ids -Timeout 15 -ErrorAction SilentlyContinue
    $left = @(Get-RustInputProcess)
    if ($left.Count -gt 0) {
        throw (T 'RustInput did not close. Close it from the tray icon and run this again.' 'O RustInput não fechou. Feche pelo ícone da bandeja e rode de novo.')
    }
    Write-Ok ((T 'closed ({0} processes)' 'fechado ({0} processos)') -f $running.Count)
}

function Test-WindowPort {
    try {
        return (@(Get-NetTCPConnection -LocalPort 5252 -State Listen -ErrorAction Stop).Count -gt 0)
    } catch {
        return $false
    }
}

# what the daemon wrote about its rights since `Since`: $true, $false, or
# $null when it did not say yet
function Get-DaemonRights([datetime]$Since) {
    $log = Join-Path $LogDir 'daemon.log'
    if (-not (Test-Path -LiteralPath $log)) { return $null }
    if ((Get-Item -LiteralPath $log).LastWriteTime -lt $Since.AddSeconds(-2)) { return $null }
    $line = Get-Content -LiteralPath $log -ErrorAction SilentlyContinue | Where-Object { $_ -match 'daemon, (with|without) administrator rights' } | Select-Object -Last 1
    if (-not $line) { return $null }
    return ($line -match 'daemon, with administrator rights')
}

# starts the service and waits for the daemon it starts in this session
function Start-RustInputService([datetime]$Since) {
    try {
        Start-Service -Name $ServiceName -ErrorAction Stop -WarningAction SilentlyContinue
    } catch {
        Write-Fail ((T 'the service did not start: {0}' 'o serviço não iniciou: {0}') -f $_.Exception.Message)
        Show-Log $ServiceLog $Since
        return $false
    }
    Write-Ok (T 'service running' 'serviço rodando')
    $deadline = (Get-Date).AddSeconds(20)
    $daemon = $null
    while ((Get-Date) -lt $deadline) {
        $daemon = Get-RustInputProcess | Where-Object { $_.Role -eq 'daemon' } | Select-Object -First 1
        if ($daemon -and (Test-WindowPort)) { break }
        Start-Sleep -Milliseconds 300
    }
    if (-not $daemon) {
        Write-Fail (T 'the service did not start the daemon. Its log:' 'o serviço não abriu o daemon. O log dele:')
        Show-Log $ServiceLog $Since
        return $false
    }
    Write-Ok ((T 'the service started the daemon (PID {0}, session {1})' 'o serviço abriu o daemon (PID {0}, sessão {1})') -f $daemon.Id, $daemon.Session)
    $rights = $null
    $deadline = (Get-Date).AddSeconds(10)
    while ($null -eq $rights -and (Get-Date) -lt $deadline) {
        $rights = Get-DaemonRights $Since
        if ($null -eq $rights) { Start-Sleep -Milliseconds 300 }
    }
    if ($rights -eq $true) {
        Write-Ok (T 'the daemon runs with administrator rights: programs run as administrator are reached too' 'o daemon roda com direitos de administrador: alcança também os programas abertos como administrador')
    } elseif ($rights -eq $false) {
        Write-Warn (T 'the daemon runs without administrator rights (is this user an administrator?)' 'o daemon roda sem direitos de administrador (este usuário é administrador?)')
    }
    return $true
}

# Starts RustInput: the service, when it is installed, and the window the way
# the Start menu would, through Explorer, so it is not tied to this window
# (closing it would close RustInput) and runs without administrator rights.
function Start-RustInput {
    $started = Get-Date
    $ok = $true
    $service = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
    if ($service) {
        $ok = Start-RustInputService $started
    }
    if ($NoStart) {
        Write-Info (T 'the window is not opened (-NoStart)' 'a janela não foi aberta (-NoStart)')
    } else {
        Start-Process -FilePath (Join-Path $env:SystemRoot 'explorer.exe') -ArgumentList ('"{0}"' -f $Exe)
        $deadline = (Get-Date).AddSeconds(20)
        while ((Get-Date) -lt $deadline) {
            $running = @(Get-RustInputProcess)
            $window = @($running | Where-Object { $_.Role -eq 'window' }).Count -gt 0
            $daemon = @($running | Where-Object { $_.Role -eq 'daemon' }).Count -gt 0
            # without the service the window starts the daemon
            if ($window -and $daemon) { break }
            Start-Sleep -Milliseconds 300
        }
    }
    # time to open the ports, or to fail
    Start-Sleep -Seconds 3
    $running = @(Get-RustInputProcess)
    if ($running.Count -eq 0) {
        Write-Fail (T 'RustInput did not stay open.' 'O RustInput não ficou aberto.')
        $ok = $false
    } else {
        $list = ($running | Sort-Object Id | ForEach-Object { '{0} {1}' -f (Get-RoleText $_.Role), $_.Id }) -join ', '
        Write-Ok ((T 'running: {0}' 'aberto: {0}') -f $list)
        Show-Ports $running
    }
    $logs = @((Join-Path $LogDir 'daemon.log'), (Join-Path $LogDir 'rustinput.log'))
    if ($service) { $logs += $ServiceLog }
    foreach ($path in $logs) {
        Show-Log $path $started
    }
    return $ok
}

function Show-Ports($Running) {
    $ids = @($Running | ForEach-Object { $_.Id })
    $udp = @()
    $tcp = @()
    try {
        $udp = @(Get-NetUDPEndpoint -ErrorAction Stop | Where-Object { $ids -contains $_.OwningProcess } |
            ForEach-Object { [int]$_.LocalPort } | Sort-Object -Unique)
        $tcp = @(Get-NetTCPConnection -State Listen -ErrorAction Stop | Where-Object { $ids -contains $_.OwningProcess } |
            ForEach-Object { [int]$_.LocalPort } | Sort-Object -Unique)
    } catch {
        return
    }
    if ($udp.Count -eq 0) {
        Write-Warn (T 'not listening for the other computers yet (no UDP port). See the log below.' 'ainda não está escutando os outros computadores (nenhuma porta UDP). Veja o log abaixo.')
        return
    }
    foreach ($port in $udp) {
        Write-Ok ((T 'UDP {0}: keyboard, mouse and clipboard' 'UDP {0}: teclado, mouse e clipboard') -f $port)
    }
    foreach ($port in $tcp) {
        if ($port -eq 5252) {
            Write-Ok (T 'TCP 5252: the RustInput window (this computer only)' 'TCP 5252: a janela do RustInput (só neste computador)')
        } else {
            Write-Ok ((T 'TCP {0}: files and pairing codes' 'TCP {0}: arquivos e códigos de autorização') -f $port)
        }
    }
}

# the end of a log RustInput wrote since `Since`, errors in red, warnings in yellow
function Show-Log([string]$Path, [datetime]$Since) {
    if (-not (Test-Path -LiteralPath $Path)) { return }
    if ((Get-Item -LiteralPath $Path).LastWriteTime -lt $Since.AddSeconds(-2)) { return }
    Write-Host ''
    Write-Host ('      {0}' -f $Path) -ForegroundColor Gray
    $lines = @(Get-Content -LiteralPath $Path -Tail 12 -ErrorAction SilentlyContinue)
    foreach ($line in $lines) {
        if ($line -match '\bERROR\b') {
            Write-Host "        $line" -ForegroundColor Red
        } elseif ($line -match '\bWARN\b') {
            Write-Host "        $line" -ForegroundColor Yellow
        } else {
            Write-Host "        $line" -ForegroundColor DarkGray
        }
    }
}

# ---------------------------------------------------------------------------
# the Windows service

# Installs the service, or brings it up to date: it starts with Windows, and
# Windows starts it again when it fails.
function Install-RustInputService {
    $command = '"{0}" windows-service' -f $Exe
    $description = T 'Keeps RustInput running in the session of the user at the screen, with administrator rights, so it reaches programs run as administrator too.' 'Mantém o RustInput rodando na sessão de quem está na tela, com direitos de administrador, para alcançar também os programas abertos como administrador.'
    $existing = Get-CimInstance -ClassName Win32_Service -Filter "Name='$ServiceName'" -ErrorAction SilentlyContinue
    if (-not $existing) {
        New-Service -Name $ServiceName -BinaryPathName $command -DisplayName 'RustInput' -Description $description -StartupType Automatic | Out-Null
        Write-Ok (T 'Windows service installed: RustInput, starts with Windows' 'serviço do Windows instalado: RustInput, inicia com o Windows')
    } else {
        if ($existing.PathName -ne $command) {
            $result = Invoke-CimMethod -InputObject $existing -MethodName Change -Arguments @{ PathName = $command }
            if ($result.ReturnValue -ne 0) {
                throw ((T 'could not change the service (error {0})' 'não deu para mudar o serviço (erro {0})') -f $result.ReturnValue)
            }
        }
        Set-Service -Name $ServiceName -StartupType Automatic -Description $description
        Write-Ok (T 'Windows service: RustInput, starts with Windows' 'serviço do Windows: RustInput, inicia com o Windows')
    }
    & sc.exe failure $ServiceName reset= 86400 actions= restart/5000/restart/10000/restart/60000 | Out-Null
    & sc.exe failureflag $ServiceName 1 | Out-Null
}

# removes the service, $true if there was one
function Remove-RustInputService {
    if (-not (Get-Service -Name $ServiceName -ErrorAction SilentlyContinue)) { return $false }
    Stop-RustInputService
    & sc.exe delete $ServiceName | Out-Null
    if ($LASTEXITCODE -ne 0) {
        Write-Warn ((T 'Windows did not remove the service (sc.exe delete: {0})' 'o Windows não removeu o serviço (sc.exe delete: {0})') -f $LASTEXITCODE)
        return $false
    }
    Write-Ok (T 'Windows service removed' 'serviço do Windows removido')
    return $true
}

# the service as asked: installed, unless -NoService or the version installed
# can not run as one
function Set-ServiceState($InstalledVersion) {
    if (-not $NoService -and (Test-ServiceVersion $InstalledVersion)) {
        Install-RustInputService
    } elseif (Remove-RustInputService) {
        if (-not $NoService) {
            Write-Info ((T 'version {0} runs without the service' 'a versão {0} roda sem o serviço') -f (Get-VersionText $InstalledVersion))
        }
    }
}

# ---------------------------------------------------------------------------
# firewall

function Convert-ProfileMask([int]$Mask) {
    $names = @()
    if ($Mask -band 1) { $names += 'Domain' }
    if ($Mask -band 2) { $names += 'Private' }
    if ($Mask -band 4) { $names += 'Public' }
    return ($names -join ', ')
}

# the networks the rules of the installer allowed, before its uninstaller
# removes them: those the rules of this script allow then
$script:KeptFirewallProfiles = ''
function Save-FirewallProfiles {
    try {
        $mask = 0
        foreach ($rule in @(Get-NetFirewallRule -Group $FirewallGroup -ErrorAction Stop)) {
            if ([string]$rule.Direction -ne 'Inbound' -or [string]$rule.Action -ne 'Allow') { continue }
            $value = [int]$rule.Profile
            if ($value -eq 0) { $script:KeptFirewallProfiles = 'Any'; return }
            $mask = $mask -bor $value
        }
        if ($mask -ne 0) { $script:KeptFirewallProfiles = Convert-ProfileMask $mask }
    } catch { }
}

# the network profiles the rules of the firewall Windows made for RustInput
# allow, when it asked: those the new rules allow too
function Get-FirewallProfiles {
    if ($script:KeptFirewallProfiles) { return $script:KeptFirewallProfiles }
    $mask = 0
    try {
        $filters = @(Get-NetFirewallApplicationFilter -ErrorAction Stop | Where-Object { $_.Program -like '*\rustinput.exe' })
        foreach ($rule in @($filters | Get-NetFirewallRule -ErrorAction SilentlyContinue)) {
            if ([string]$rule.Direction -ne 'Inbound' -or [string]$rule.Action -ne 'Allow' -or [string]$rule.Enabled -ne 'True') { continue }
            if ($rule.Group -eq $FirewallGroup) { continue }
            $value = [int]$rule.Profile
            if ($value -eq 0) { return 'Any' }
            $mask = $mask -bor $value
        }
    } catch { }
    if ($mask -eq 0) { return 'Domain, Private' }
    return (Convert-ProfileMask $mask)
}

# Lets the other computers reach RustInput where it is installed now: Windows
# would otherwise ask again for the new folder, in a window easy to miss.
function Install-FirewallRules {
    if (-not (Test-Administrator)) { return }
    try {
        $ours = @(Get-NetFirewallRule -Group $FirewallGroup -ErrorAction SilentlyContinue)
        $current = @($ours | Get-NetFirewallApplicationFilter -ErrorAction SilentlyContinue | Where-Object { $_.Program -eq $Exe })
        if ($ours.Count -gt 0 -and $current.Count -eq $ours.Count) {
            Write-Ok (T 'Windows firewall: the other computers reach RustInput' 'firewall do Windows: os outros computadores alcançam o RustInput')
            return
        }
        $profiles = Get-FirewallProfiles
        $ours | Remove-NetFirewallRule -ErrorAction SilentlyContinue
        foreach ($protocol in 'UDP', 'TCP') {
            New-NetFirewallRule -DisplayName "RustInput ($protocol)" -Group $FirewallGroup -Direction Inbound -Action Allow -Program $Exe -Protocol $protocol -Profile $profiles | Out-Null
        }
        Write-Ok ((T 'Windows firewall: the other computers reach RustInput (networks: {0})' 'firewall do Windows: os outros computadores alcançam o RustInput (redes: {0})') -f $profiles)
    } catch {
        Write-Warn ((T 'could not set up the Windows firewall: {0}' 'não deu para configurar o firewall do Windows: {0}') -f $_.Exception.Message)
    }
}

# ---------------------------------------------------------------------------
# the installer (rustinput-windows-x86_64-setup.exe, since 0.24.0)

# The installation the installer made, as Windows lists it in Settings > Apps:
# $null when there is none. The 64-bit registry also from a 32-bit PowerShell.
function Get-SetupInstall {
    try {
        $hive = [Microsoft.Win32.RegistryKey]::OpenBaseKey([Microsoft.Win32.RegistryHive]::LocalMachine, [Microsoft.Win32.RegistryView]::Registry64)
        $key = $hive.OpenSubKey($SetupKey)
        if (-not $key) { return $null }
        try {
            $line = [string]$key.GetValue('UninstallString')
            $uninstaller = $line
            if ($line -match '^\s*"([^"]+)"') { $uninstaller = $Matches[1] }
            return New-Object PSObject -Property @{
                Folder = ([string]$key.GetValue('InstallLocation')).TrimEnd('\')
                Version = [string]$key.GetValue('DisplayVersion')
                Uninstaller = $uninstaller
            }
        } finally {
            $key.Close()
        }
    } catch {
        return $null
    }
}

# Removes the installation of the installer with its uninstaller, which
# removes the service, the firewall rules, the Start menu items and the
# folders, and keeps the settings. $true if there was one.
function Remove-SetupInstall {
    $install = Get-SetupInstall
    if (-not $install) { return $false }
    if (-not $install.Uninstaller -or -not (Test-Path -LiteralPath $install.Uninstaller)) {
        throw ((T 'the uninstaller of RustInput is missing ({0}): uninstall it in Settings > Apps' 'falta o desinstalador do RustInput ({0}): desinstale em Configurações > Aplicativos') -f $install.Uninstaller)
    }
    Write-Info ((T 'removing the installation of the installer ({0}), the settings stay...' 'removendo a instalação do instalador ({0}), as configurações ficam...') -f $install.Version)
    [void](Start-Process -FilePath $install.Uninstaller -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART' -Wait -PassThru)
    # it goes on in a copy of itself: done once Windows no longer lists it
    # and its file is gone
    $deadline = (Get-Date).AddSeconds(120)
    while (((Get-SetupInstall) -or (Test-Path -LiteralPath $install.Uninstaller)) -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 500
    }
    if (Get-SetupInstall) {
        throw (T 'the uninstaller of RustInput did not finish: uninstall it in Settings > Apps' 'o desinstalador do RustInput não terminou: desinstale em Configurações > Aplicativos')
    }
    Write-Ok (T 'removed the installation of the installer' 'removida a instalação do instalador')
    return $true
}

# the files of the installed copy into `Destination`, without the uninstaller
function Copy-Install([string]$Destination) {
    Remove-Folder $Destination
    New-Item -ItemType Directory -Force -Path $Destination | Out-Null
    Get-ChildItem -LiteralPath $InstallDir -Force | Where-Object { $_.Name -notlike 'unins*' } |
        Copy-Item -Destination $Destination -Recurse -Force
}

# Turns the installation of the installer into one of this script, with the
# same files, for a version without the installer (-Version 0.23.1, -Zip,
# -Restore): its uninstaller removes the service, the firewall rules and the
# Start menu items, which this script then makes again. The copy of the
# version before stays.
function Convert-SetupInstall {
    if (-not (Get-SetupInstall)) { return }
    Save-FirewallProfiles
    $aside = "$InstallDir.restore"
    $previousAside = "$PreviousDir.restore"
    $hadFiles = Test-Path -LiteralPath $Exe
    if ($hadFiles) { Copy-Install $aside }
    Remove-Folder $previousAside
    if (Test-Path -LiteralPath $PreviousDir) { Move-Item -LiteralPath $PreviousDir -Destination $previousAside }
    try {
        [void](Remove-SetupInstall)
    } finally {
        if ($hadFiles) {
            Remove-Folder $InstallDir
            Move-Item -LiteralPath $aside -Destination $InstallDir
        }
        if (Test-Path -LiteralPath $previousAside) {
            Remove-Folder $PreviousDir
            Move-Item -LiteralPath $previousAside -Destination $PreviousDir
        }
    }
}

# Runs the installer without questions (this script runs as administrator).
# The choices of the last installation stay, unless -NoService or
# -NoShortcuts turn some off.
function Invoke-Setup([string]$SetupPath) {
    $log = Join-Path $LogDir 'setup-install.log'
    $arguments = @('/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/SP-', ('/LOG="{0}"' -f $log))
    $tasks = @()
    if ($NoService) { $tasks += '!service' }
    if ($NoShortcuts) { $tasks += '!tray', '!desktopicon' }
    if ($tasks.Count -gt 0) { $arguments += ('/MERGETASKS="{0}"' -f ($tasks -join ',')) }
    Write-Detail ('{0} {1}' -f (Split-Path -Leaf $SetupPath), ($arguments -join ' '))
    $process = Start-Process -FilePath $SetupPath -ArgumentList $arguments -Wait -PassThru
    $code = $process.ExitCode
    $setupLog = Join-Path $ServiceLogDir 'setup.log'
    if ($code -eq 0) {
        Write-Ok (T 'the installer is done (Settings > Apps lists RustInput)' 'o instalador terminou (Configurações > Aplicativos mostra o RustInput)')
        return
    }
    if ($code -ge 100) {
        Write-Warn ((T 'installed, but setting up the service or the firewall failed (code {0})' 'instalado, mas a configuração do serviço ou do firewall falhou (código {0})') -f ($code - 100))
        Show-Log $setupLog (Get-Date).AddMinutes(-10)
        return
    }
    Show-Log $log (Get-Date).AddMinutes(-10)
    throw ((T 'the installer failed (code {0}), see {1}' 'o instalador falhou (código {0}), veja {1}') -f $code, $log)
}

# ---------------------------------------------------------------------------
# files

function Remove-Folder([string]$Path) {
    if (Test-Path -LiteralPath $Path) {
        Remove-Item -LiteralPath $Path -Recurse -Force
    }
}

# Puts the unpacked release in place of the installed copy, which becomes the
# previous one. Folders are renamed, so a failure leaves the old copy working.
function Install-Staged {
    Remove-Folder $PreviousDir
    $hadInstall = Test-Path -LiteralPath $InstallDir
    if ($hadInstall) {
        try {
            Move-Item -LiteralPath $InstallDir -Destination $PreviousDir
        } catch {
            # a program has a file of the folder open (a terminal inside it...):
            # copy over it instead
            Write-Warn (T 'the folder is in use, copying the files over it' 'a pasta está em uso, copiando os arquivos por cima')
            New-Item -ItemType Directory -Force -Path $PreviousDir | Out-Null
            Copy-Item -Path (Join-Path $InstallDir '*') -Destination $PreviousDir -Recurse -Force
            Copy-Item -Path (Join-Path $StagingDir '*') -Destination $InstallDir -Recurse -Force
            Remove-Folder $StagingDir
            return
        }
    }
    try {
        Move-Item -LiteralPath $StagingDir -Destination $InstallDir
    } catch {
        if ($hadInstall) {
            Move-Item -LiteralPath $PreviousDir -Destination $InstallDir
        }
        throw
    }
}

# whether this run moves RustInput from where versions before 0.23.0 were
# installed (%LOCALAPPDATA%\Programs\RustInput)
function Test-Moving {
    if ($InstallDir -eq $OldDefaultDir) { return $false }
    if (Test-Path -LiteralPath $Exe) { return $false }
    return (Test-Path -LiteralPath (Join-Path $OldDefaultDir 'rustinput.exe'))
}

# The copy in the old folder becomes the previous version of the new one,
# which -Restore goes back to, and the old folder goes away.
function Move-OldInstall {
    Remove-Folder $PreviousDir
    Copy-Item -LiteralPath $OldDefaultDir -Destination $PreviousDir -Recurse -Force
    foreach ($dir in $OldDefaultDir, "$OldDefaultDir.previous", "$OldDefaultDir.new") {
        try { Remove-Folder $dir } catch { Write-Warn ((T 'could not remove {0}: {1}' 'não deu para remover {0}: {1}') -f $dir, $_.Exception.Message) }
    }
    Write-Ok ((T 'moved here from {0}; that copy is the previous version now' 'mudou de {0} para cá; aquela cópia virou a versão anterior') -f $OldDefaultDir)
}

# this script next to RustInput, for the Start menu item, when the release
# does not bring it (versions before 0.20.1)
function Install-Self {
    $installed = Join-Path $InstallDir $ScriptName
    if ((Test-Path -LiteralPath $installed) -or -not $SelfBytes) { return }
    try { [System.IO.File]::WriteAllBytes($installed, $SelfBytes) } catch { }
}

function New-Shortcut([string]$Path, [string]$Target, [string]$Arguments, [string]$Description) {
    $shell = New-Object -ComObject WScript.Shell
    $link = $shell.CreateShortcut($Path)
    $link.TargetPath = $Target
    $link.Arguments = $Arguments
    $link.WorkingDirectory = $env:USERPROFILE
    $link.IconLocation = "$Exe,0"
    $link.Description = $Description
    $link.Save()
}

function Get-ShortcutPaths {
    $programs = [Environment]::GetFolderPath('Programs')
    return @(
        (Join-Path $programs 'RustInput.lnk'),
        (Join-Path $programs 'Update RustInput.lnk'),
        (Join-Path $programs 'Atualizar o RustInput.lnk')
    )
}

function Install-Shortcuts {
    if ($NoShortcuts) { return }
    $programs = [Environment]::GetFolderPath('Programs')
    $updater = Join-Path $InstallDir $ScriptName
    try {
        New-Shortcut (Join-Path $programs 'RustInput.lnk') $Exe '' 'RustInput'
        $made = 'RustInput'
        if (Test-Path -LiteralPath $updater) {
            $name = T 'Update RustInput' 'Atualizar o RustInput'
            $powershell = Join-Path $env:SystemRoot 'System32\WindowsPowerShell\v1.0\powershell.exe'
            New-Shortcut (Join-Path $programs "$name.lnk") $powershell ('-NoProfile -ExecutionPolicy Bypass -File "{0}"' -f $updater) $name
            $made += ", $name"
        }
        Write-Ok ((T 'Start menu: {0}' 'Menu Iniciar: {0}') -f $made)
    } catch {
        Write-Warn ((T 'could not create the Start menu items: {0}' 'não deu para criar os itens do menu Iniciar: {0}') -f $_.Exception.Message)
    }
}

# ---------------------------------------------------------------------------

function Show-Summary([string]$Before, [string]$After, [bool]$Running) {
    Write-Host ''
    Write-Host ('-' * 72) -ForegroundColor DarkGray
    if ($Running) {
        Write-Host ('  RustInput {0} ' -f $After) -ForegroundColor Green -NoNewline
        Write-Host (T 'is installed and running.' 'está instalado e aberto.') -ForegroundColor Green
    } else {
        Write-Host ('  RustInput {0} ' -f $After) -ForegroundColor Yellow -NoNewline
        Write-Host (T 'is installed.' 'está instalado.') -ForegroundColor Yellow
    }
    if ($Before) {
        Write-Info ((T 'before: {0}' 'antes: {0}') -f $Before)
    }
    Write-Info ((T 'folder:            {0}' 'pasta:             {0}') -f $InstallDir)
    if (Get-SetupInstall) {
        Write-Info (T 'installed with:    the installer, uninstalled in Settings > Apps' 'instalado com:     o instalador, desinstala em Configurações > Aplicativos')
    }
    $service = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
    if ($service) {
        Write-Info ((T 'Windows service:   {0} ({1})' 'serviço:           {0} ({1})') -f $ServiceName, $service.Status)
    } else {
        Write-Info (T 'Windows service:   none, without administrator rights' 'serviço:           nenhum, sem direitos de administrador')
    }
    if (Test-Path -LiteralPath $PreviousDir) {
        Write-Info ((T 'previous version:  {0}' 'versão anterior:   {0}') -f $PreviousDir)
        Write-Detail ((T 'to go back to it:  powershell -ExecutionPolicy Bypass -File "{0}" -Restore' 'para voltar a ela: powershell -ExecutionPolicy Bypass -File "{0}" -Restore') -f (Join-Path $InstallDir $ScriptName))
    }
    Write-Info ((T 'RustInput logs:    {0}' 'logs do RustInput: {0}') -f $LogDir)
    if ($service) {
        Write-Info ((T 'service log:       {0}' 'log do serviço:    {0}') -f $ServiceLog)
    }
    Write-Info ((T 'this update:       {0}' 'esta atualização:  {0}') -f $UpdateLog)
    Write-Host ('-' * 72) -ForegroundColor DarkGray
}

function Invoke-Restore {
    $script:StepCount = 2
    if (-not (Test-Path -LiteralPath $PreviousDir)) {
        throw ((T 'there is no previous version in {0}' 'não há versão anterior em {0}') -f $PreviousDir)
    }
    $before = Get-ExeVersion $Exe
    $back = Get-ExeVersion (Join-Path $PreviousDir 'rustinput.exe')
    Write-Step (T 'Closing RustInput' 'Fechando o RustInput')
    Stop-RustInput
    Write-Step ((T 'Going back to {0}' 'Voltando para {0}') -f (Get-VersionText $back))
    Convert-SetupInstall
    $swap = "$InstallDir.swap"
    Remove-Folder $swap
    if (Test-Path -LiteralPath $InstallDir) {
        Move-Item -LiteralPath $InstallDir -Destination $swap
    }
    Move-Item -LiteralPath $PreviousDir -Destination $InstallDir
    if (Test-Path -LiteralPath $swap) {
        Move-Item -LiteralPath $swap -Destination $PreviousDir
    }
    Install-Self
    Write-Ok ((T 'installed {0}, the other one is now the previous version' 'instalada a {0}; a outra virou a versão anterior') -f (Get-VersionText $back))
    Set-ServiceState $back
    Install-FirewallRules
    Install-Shortcuts
    $running = Start-RustInput
    Show-Summary (Get-VersionText $before) (Get-VersionText $back) $running
}

function Invoke-Uninstall {
    $script:StepCount = 2
    Write-Step (T 'Closing RustInput' 'Fechando o RustInput')
    Stop-RustInput
    Write-Step (T 'Removing RustInput' 'Removendo o RustInput')
    [void](Remove-SetupInstall)
    [void](Remove-RustInputService)
    foreach ($dir in $InstallDir, $PreviousDir, $StagingDir, "$InstallDir.swap", $OldDefaultDir, "$OldDefaultDir.previous", "$OldDefaultDir.new", $ServiceLogDir) {
        if (Test-Path -LiteralPath $dir) {
            try {
                Remove-Folder $dir
                Write-Ok ((T 'removed {0}' 'removido {0}') -f $dir)
            } catch {
                Write-Warn ((T 'could not remove {0}: {1}' 'não deu para remover {0}: {1}') -f $dir, $_.Exception.Message)
            }
        }
    }
    $links = @(Get-ShortcutPaths | Where-Object { Test-Path -LiteralPath $_ })
    foreach ($link in $links) { Remove-Item -LiteralPath $link -Force }
    if ($links.Count -gt 0) { Write-Ok (T 'Start menu items removed' 'itens do menu Iniciar removidos') }
    if (Test-Path -LiteralPath $MenuKey) {
        Remove-Item -LiteralPath $MenuKey -Recurse -Force
        Write-Ok (T '"Copy to RustInput" removed from Explorer' '"Copiar para o RustInput" removido do Explorer')
    }
    if (Test-Administrator) {
        $rules = @(Get-NetFirewallRule -Group $FirewallGroup -ErrorAction SilentlyContinue)
        if ($rules.Count -gt 0) {
            $rules | Remove-NetFirewallRule
            Write-Ok (T 'Windows firewall rules removed' 'regras do firewall do Windows removidas')
        }
    }
    Write-Host ''
    Write-Host ('-' * 72) -ForegroundColor DarkGray
    Write-Host (T '  RustInput is removed.' '  O RustInput foi removido.') -ForegroundColor Green
    Write-Info ((T 'kept, the settings and logs: {0}' 'ficaram as configurações e os logs: {0}') -f (Join-Path $env:LOCALAPPDATA 'rustinput'))
    Write-Detail ((T 'to remove them too: Remove-Item -Recurse "{0}"' 'para remover também: Remove-Item -Recurse "{0}"') -f (Join-Path $env:LOCALAPPDATA 'rustinput'))
    Write-Host ('-' * 72) -ForegroundColor DarkGray
}

# $null (not installed) and '' (unknown) differ: no [string], which turns
# $null into ''
function Get-VersionText($Version) {
    if ($null -eq $Version) { return (T 'not installed' 'não instalado') }
    if ($Version -eq '') { return (T 'unknown (before 0.20.1)' 'desconhecida (anterior à 0.20.1)') }
    return $Version
}

# Puts the unpacked release in place, then the service, the firewall and the
# Start menu, and starts RustInput.
# $Installed untyped: $null (not installed) must not become '' (unknown)
function Complete-Install($Installed, [string]$Target, [bool]$Moving) {
    Write-Step (T 'Closing RustInput' 'Fechando o RustInput')
    Stop-RustInput
    # a version without the installer, over one it installed
    Convert-SetupInstall

    Write-Step ((T 'Installing {0}' 'Instalando a {0}') -f $Target)
    Install-Staged
    if ($Moving) { Move-OldInstall }
    Install-Self
    $now = Get-ExeVersion $Exe
    if ($now) {
        Write-Ok ((T 'installed: rustinput.exe {0}' 'instalado: rustinput.exe {0}') -f $now)
    } else {
        Write-Ok ((T 'installed in {0}' 'instalado em {0}') -f $InstallDir)
    }
    Set-ServiceState $now
    Install-FirewallRules
    Install-Shortcuts

    Write-Step (T 'Starting RustInput' 'Abrindo o RustInput')
    $running = Start-RustInput
    if (-not $running -and (Test-Path -LiteralPath $PreviousDir)) {
        Write-Warn (T 'to go back to the previous version, run this again with -Restore' 'para voltar para a versão anterior, rode de novo com -Restore')
    }
    Show-Summary (Get-VersionText $Installed) $Target $running
}

# Runs the installer of a release, keeping a copy of the installed version
# for -Restore, and starts RustInput.
function Complete-SetupInstall($Installed, [string]$Target, [string]$SetupPath, [bool]$Moving) {
    Write-Step (T 'Closing RustInput' 'Fechando o RustInput')
    Stop-RustInput

    Write-Step ((T 'Installing {0} with its installer' 'Instalando a {0} com o instalador') -f $Target)
    if (Test-Path -LiteralPath $Exe) {
        Copy-Install $PreviousDir
        Write-Ok ((T 'kept a copy of the version before: {0}' 'guardada uma cópia da versão anterior: {0}') -f $PreviousDir)
    }
    if ($Moving) { Move-OldInstall }
    Invoke-Setup $SetupPath
    $install = Get-SetupInstall
    if ($install -and $install.Folder -and $install.Folder -ne $InstallDir) {
        Write-Info ((T 'the installer keeps RustInput in {0}' 'o instalador mantém o RustInput em {0}') -f $install.Folder)
        $script:InstallDir = $install.Folder
        $script:Exe = Join-Path $install.Folder 'rustinput.exe'
    }
    $now = Get-ExeVersion $Exe
    if ($now) {
        Write-Ok ((T 'installed: rustinput.exe {0}' 'instalado: rustinput.exe {0}') -f $now)
    }

    Write-Step (T 'Starting RustInput' 'Abrindo o RustInput')
    $running = Start-RustInput
    if (-not $running -and (Test-Path -LiteralPath $PreviousDir)) {
        Write-Warn (T 'to go back to the previous version, run this again with -Restore' 'para voltar para a versão anterior, rode de novo com -Restore')
    }
    Show-Summary (Get-VersionText $Installed) $Target $running
}

# runs an installer at hand (-Setup), without GitHub
function Invoke-SetupFileInstall {
    $script:StepCount = 3
    $moving = Test-Moving
    $installed = Get-ExeVersion $Exe
    if ($moving) { $installed = Get-ExeVersion (Join-Path $OldDefaultDir 'rustinput.exe') }
    $setupPath = (Resolve-Path -LiteralPath $Setup).Path
    $target = [string](Get-Item -LiteralPath $setupPath).VersionInfo.ProductVersion
    Write-Info ((T 'installed:  {0}' 'instalada:  {0}') -f (Get-VersionText $installed))
    Write-Info ((T 'installer:  {0} ({1})' 'instalador: {0} ({1})') -f $setupPath, $target)
    Write-Detail ('SHA-256: {0}' -f (Get-FileHash -Algorithm SHA256 -LiteralPath $setupPath).Hash.ToLowerInvariant())
    Complete-SetupInstall $installed $target $setupPath $moving
}

# installs a zip at hand (-Zip), without GitHub
function Invoke-ZipInstall {
    $script:StepCount = 4
    $moving = Test-Moving
    $installed = Get-ExeVersion $Exe
    if ($moving) { $installed = Get-ExeVersion (Join-Path $OldDefaultDir 'rustinput.exe') }
    $zipPath = (Resolve-Path -LiteralPath $Zip).Path
    Write-Step ((T 'Unpacking {0}' 'Descompactando {0}') -f $zipPath)
    Write-Info ((T 'installed:  {0}' 'instalada:  {0}') -f (Get-VersionText $installed))
    $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $zipPath).Hash.ToLowerInvariant()
    Write-Detail ('SHA-256: {0}' -f $hash)
    $target = Expand-Release $zipPath
    Complete-Install $installed (Get-VersionText $target) $moving
}

function Invoke-Update {
    $moving = Test-Moving
    $installed = Get-ExeVersion $Exe
    if ($moving) { $installed = Get-ExeVersion (Join-Path $OldDefaultDir 'rustinput.exe') }

    Write-Step (T 'Looking for the version on GitHub' 'Procurando a versão no GitHub')
    $release = Get-Release $Version
    $target = ([string]$release.tag_name).TrimStart('v')
    # the installer since 0.24.0, the zip before
    $asset = $release.assets | Where-Object { $_.name -eq $SetupAssetName } | Select-Object -First 1
    $withSetup = [bool]$asset
    if (-not $asset) {
        $asset = $release.assets | Where-Object { $_.name -eq $AssetName } | Select-Object -First 1
    }
    if (-not $asset) {
        throw ((T 'release {0} has no {1}' 'a versão {0} não tem o {1}') -f $target, $AssetName)
    }
    Write-Info ((T 'installed:  {0}' 'instalada:  {0}') -f (Get-VersionText $installed))
    if ($moving) {
        Write-Detail ((T 'in {0}, moves to {1}' 'em {0}, muda para {1}') -f $OldDefaultDir, $InstallDir)
    }
    Write-Info ((T 'on GitHub:  {0}  (published {1}, {2})' 'no GitHub:  {0}  (publicada em {1}, {2})') -f $target, (Format-Date $release.published_at), (Format-Size $asset.size))
    Write-Detail $release.html_url

    $installedVersion = ConvertTo-Version $installed
    $targetVersion = ConvertTo-Version $target
    if (-not $Force -and -not $moving -and $installedVersion -and $targetVersion) {
        $same = $installedVersion -eq $targetVersion
        $newer = ($Version -eq 'latest') -and ($installedVersion -gt $targetVersion)
        if ($same -or $newer) {
            if ($same) {
                Write-Ok (T 'already installed, nothing to do (-Force installs it again)' 'já está instalada, nada a fazer (-Force instala de novo)')
            } else {
                Write-Ok (T 'the installed version is newer than the published one (-Force installs it anyway)' 'a versão instalada é mais nova que a publicada (-Force instala mesmo assim)')
            }
            # the installer keeps its own installation in order
            if (-not (Get-SetupInstall)) {
                Install-Self
                Set-ServiceState $installed
                Install-FirewallRules
                Install-Shortcuts
            }
            $service = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
            $daemon = @(Get-RustInputProcess | Where-Object { $_.Role -eq 'daemon' }).Count -gt 0
            $running = $daemon -and (-not $service -or $service.Status -eq 'Running')
            if (-not $running) {
                Write-Host ''
                Write-Info (T 'RustInput is not running, starting it:' 'O RustInput não está aberto, abrindo:')
                $running = Start-RustInput
            }
            Show-Summary '' $installed $running
            return
        }
    }

    # what changes: from the installed version, or else since the release before
    if ($installedVersion -and $targetVersion -and $installedVersion -lt $targetVersion) {
        $changes = @(Get-Changes "v$installed" "v$target")
    } elseif ([string]$release.body -match 'compare/(\S+?)\.\.\.(\S+)') {
        $changes = @(Get-Changes $Matches[1] $Matches[2])
    } else {
        $changes = @()
    }
    if ($changes.Count -gt 0) {
        Write-Info (T 'what changed:' 'o que mudou:')
        $changes | Select-Object -First 20 | ForEach-Object { Write-Detail "  - $_" }
        if ($changes.Count -gt 20) {
            Write-Detail ((T '  ... and {0} more' '  ... e mais {0}') -f ($changes.Count - 20))
        }
    }

    Write-Step ((T 'Downloading {0}' 'Baixando {0}') -f $asset.name)
    $zip = Join-Path $env:TEMP ('rustinput-{0}-{1}' -f $target, $asset.name)
    $download = Save-File $asset.browser_download_url $zip ([long]$asset.size) $asset.name
    Write-Ok ('{0} {1} {2:N1} s ({3}/s)' -f (Format-Size $download.Bytes), (T 'in' 'em'), $download.Seconds, (Format-Size ($download.Bytes / [Math]::Max($download.Seconds, 0.001))))

    Write-Step (T 'Checking the download' 'Conferindo o download')
    if ($download.Bytes -ne [long]$asset.size) {
        throw ((T 'the download is incomplete: {0} of {1}' 'o download veio incompleto: {0} de {1}') -f (Format-Size $download.Bytes), (Format-Size $asset.size))
    }
    $hash = (Get-FileHash -Algorithm SHA256 -LiteralPath $zip).Hash.ToLowerInvariant()
    $digest = $null
    if ($asset.PSObject.Properties['digest'] -and ([string]$asset.digest) -like 'sha256:*') {
        $digest = ([string]$asset.digest).Substring(7).ToLowerInvariant()
    }
    if ($digest) {
        if ($hash -ne $digest) {
            throw ((T 'the SHA-256 does not match GitHub ({0} instead of {1}): the download is damaged' 'o SHA-256 não confere com o GitHub ({0} em vez de {1}): o download veio estragado') -f $hash, $digest)
        }
        Write-Ok ((T 'SHA-256 matches GitHub: {0}' 'SHA-256 confere com o GitHub: {0}') -f $hash)
    } else {
        Write-Warn ((T 'GitHub does not list the SHA-256 of this file; SHA-256: {0}' 'o GitHub não informa o SHA-256 deste arquivo; SHA-256: {0}') -f $hash)
    }
    if ($withSetup) {
        try {
            Complete-SetupInstall $installed $target $zip $moving
        } finally {
            Remove-Item -LiteralPath $zip -Force -ErrorAction SilentlyContinue
        }
        return
    }
    [void](Expand-Release $zip)
    Remove-Item -LiteralPath $zip -Force -ErrorAction SilentlyContinue

    Complete-Install $installed $target $moving
}

# ---------------------------------------------------------------------------

if ((Test-NeedsAdministrator) -and -not (Test-Administrator)) {
    Invoke-Elevated
}

try {
    New-Item -ItemType Directory -Force -Path $LogDir | Out-Null
    Start-Transcript -Path $UpdateLog -Append | Out-Null
    $script:TranscriptOn = $true
} catch { }

try {
    # Windows PowerShell 5.1 may still default to older TLS versions
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} catch { }

Write-Banner
$rights = T 'without administrator rights' 'sem direitos de administrador'
if (Test-Administrator) { $rights = T 'as administrator' 'como administrador' }
Write-Detail ('PowerShell {0}, {1}, {2}' -f $PSVersionTable.PSVersion, (Get-Date).ToString('g'), $rights)

try {
    if ($Uninstall) {
        Invoke-Uninstall
    } elseif ($Restore) {
        Invoke-Restore
    } elseif ($Setup) {
        Invoke-SetupFileInstall
    } elseif ($Zip) {
        Invoke-ZipInstall
    } else {
        Invoke-Update
    }
} catch {
    Write-Host ''
    Write-Fail $_.Exception.Message
    if ($_.InvocationInfo -and $_.InvocationInfo.PositionMessage) {
        Write-Detail ((T 'details: {0}' 'detalhes: {0}') -f $_.InvocationInfo.PositionMessage.Split("`n")[0].Trim())
    }
    Write-Info ((T 'log of this update: {0}' 'log desta atualização: {0}') -f $UpdateLog)
    Exit-Script 1
}
Exit-Script 0
