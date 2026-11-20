<#
.SYNOPSIS
    Installs or updates RustInput on Windows.

.DESCRIPTION
    Finds the newest release of RustInput on GitHub, downloads it with a
    progress bar, checks its SHA-256, closes RustInput, replaces the installed
    copy while keeping the previous one, starts RustInput again and shows
    whether it came up, with the end of its logs.

    Everything shown is also written to %LOCALAPPDATA%\rustinput\logs\update.log.
    Messages are in Portuguese when Windows is, in English otherwise.

.PARAMETER Version
    The version to install, like 0.20.1. The newest one by default. Also
    installs an older version, to go back to it.

.PARAMETER Force
    Installs even when that version is already installed.

.PARAMETER Restore
    Goes back to the copy the last update kept (running it again undoes it).

.PARAMETER NoStart
    Does not start RustInput afterwards.

.PARAMETER NoShortcuts
    Does not create the "RustInput" and "Update RustInput" Start menu items.

.PARAMETER NoPause
    Does not wait for Enter before the window closes.

.PARAMETER InstallDir
    Where RustInput is installed, %LOCALAPPDATA%\Programs\RustInput by default.

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
    [switch]$NoStart,
    [switch]$NoShortcuts,
    [switch]$NoPause,
    [string]$InstallDir = ''
)

$ErrorActionPreference = 'Stop'
# the progress of the cmdlets slows Windows PowerShell down; the download
# shows its own
$ProgressPreference = 'SilentlyContinue'

$Repo = 'felipemarques/rust-input'
$AssetName = 'rustinput-windows-x86_64.zip'
$ScriptName = 'update-rustinput.ps1'
$UserAgent = 'rustinput-updater'
if (-not $InstallDir) {
    $InstallDir = Join-Path $env:LOCALAPPDATA 'Programs\RustInput'
}
$InstallDir = $InstallDir.TrimEnd('\')
$Exe = Join-Path $InstallDir 'rustinput.exe'
$PreviousDir = "$InstallDir.previous"
$StagingDir = "$InstallDir.new"
$LogDir = Join-Path $env:LOCALAPPDATA 'rustinput\logs'
$UpdateLog = Join-Path $LogDir 'update.log'

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

function Get-Release([string]$Wanted) {
    $api = "https://api.github.com/repos/$Repo/releases/"
    if ($Wanted -eq 'latest') {
        $url = $api + 'latest'
    } else {
        $url = $api + 'tags/v' + $Wanted.TrimStart('v')
    }
    try {
        return Invoke-RestMethod -Uri $url -UserAgent $UserAgent -TimeoutSec 30
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
        $compare = Invoke-RestMethod -Uri "https://api.github.com/repos/$Repo/compare/$From...$To" -UserAgent $UserAgent -TimeoutSec 30
    } catch {
        return @()
    }
    $subjects = @($compare.commits | ForEach-Object { ($_.commit.message -split "`n")[0].Trim() })
    [array]::Reverse($subjects)
    return @($subjects | Where-Object { $_ -and $_ -notmatch '^chore: bump version' })
}

# ---------------------------------------------------------------------------
# download

function Save-File([string]$Url, [string]$Path, [long]$ExpectedSize) {
    # shown here only (see the top)
    $ProgressPreference = 'Continue'
    Add-Type -AssemblyName System.Net.Http
    $client = New-Object System.Net.Http.HttpClient
    $client.Timeout = [TimeSpan]::FromMinutes(30)
    $client.DefaultRequestHeaders.UserAgent.ParseAdd($UserAgent)
    $activity = (T 'Downloading ' 'Baixando ') + $AssetName
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

# ---------------------------------------------------------------------------
# processes

function Get-RustInputProcess {
    return @(Get-Process -Name rustinput -ErrorAction SilentlyContinue)
}

function Stop-RustInput {
    $running = Get-RustInputProcess
    if ($running.Count -eq 0) {
        Write-Info (T 'RustInput is not running.' 'O RustInput não está aberto.')
        return
    }
    foreach ($process in $running) {
        $path = $process.Path
        if (-not $path) { $path = '?' }
        Write-Detail ('PID {0}  {1}' -f $process.Id, $path)
    }
    $running | Stop-Process -Force -ErrorAction SilentlyContinue
    $running | Wait-Process -Timeout 15 -ErrorAction SilentlyContinue
    $left = Get-RustInputProcess
    if ($left.Count -gt 0) {
        throw (T 'RustInput did not close. Close it from the tray icon and run this again.' 'O RustInput não fechou. Feche pelo ícone da bandeja e rode de novo.')
    }
    Write-Ok ((T 'closed ({0} processes)' 'fechado ({0} processos)') -f $running.Count)
}

# Starts RustInput the way the Start menu would: through Explorer, so it is not
# tied to this window (closing it would close RustInput) and logs to its files.
function Start-RustInput {
    $started = Get-Date
    Start-Process -FilePath (Join-Path $env:SystemRoot 'explorer.exe') -ArgumentList ('"{0}"' -f $Exe)
    # the window starts the service, a second rustinput.exe
    $deadline = (Get-Date).AddSeconds(20)
    while ((Get-Date) -lt $deadline -and (Get-RustInputProcess).Count -lt 2) {
        Start-Sleep -Milliseconds 300
    }
    # time to open the ports, or to fail
    Start-Sleep -Seconds 3
    $running = Get-RustInputProcess
    if ($running.Count -eq 0) {
        Write-Fail (T 'RustInput did not stay open.' 'O RustInput não ficou aberto.')
    } else {
        $ids = ($running | ForEach-Object { $_.Id }) -join ', '
        Write-Ok ((T 'running (PID {0})' 'aberto (PID {0})') -f $ids)
        Show-Ports $running
    }
    foreach ($name in 'daemon.log', 'rustinput.log') {
        Show-Log (Join-Path $LogDir $name) $started
    }
    return ($running.Count -gt 0)
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
    if (Test-Path -LiteralPath $PreviousDir) {
        Write-Info ((T 'previous version:  {0}' 'versão anterior:   {0}') -f $PreviousDir)
        Write-Detail ((T 'to go back to it:  powershell -ExecutionPolicy Bypass -File "{0}" -Restore' 'para voltar a ela: powershell -ExecutionPolicy Bypass -File "{0}" -Restore') -f (Join-Path $InstallDir $ScriptName))
    }
    Write-Info ((T 'RustInput logs:    {0}' 'logs do RustInput: {0}') -f $LogDir)
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
    $running = $false
    if (-not $NoStart) {
        $running = Start-RustInput
    }
    Show-Summary (Get-VersionText $before) (Get-VersionText $back) $running
}

# $null (not installed) and '' (unknown) differ: no [string], which turns
# $null into ''
function Get-VersionText($Version) {
    if ($null -eq $Version) { return (T 'not installed' 'não instalado') }
    if ($Version -eq '') { return (T 'unknown (before 0.20.1)' 'desconhecida (anterior à 0.20.1)') }
    return $Version
}

function Invoke-Update {
    $installed = Get-ExeVersion $Exe

    Write-Step (T 'Looking for the version on GitHub' 'Procurando a versão no GitHub')
    $release = Get-Release $Version
    $target = ([string]$release.tag_name).TrimStart('v')
    $asset = $release.assets | Where-Object { $_.name -eq $AssetName } | Select-Object -First 1
    if (-not $asset) {
        throw ((T 'release {0} has no {1}' 'a versão {0} não tem o {1}') -f $target, $AssetName)
    }
    Write-Info ((T 'installed:  {0}' 'instalada:  {0}') -f (Get-VersionText $installed))
    Write-Info ((T 'on GitHub:  {0}  (published {1}, {2})' 'no GitHub:  {0}  (publicada em {1}, {2})') -f $target, (Format-Date $release.published_at), (Format-Size $asset.size))
    Write-Detail $release.html_url

    $installedVersion = ConvertTo-Version $installed
    $targetVersion = ConvertTo-Version $target
    if (-not $Force -and $installedVersion -and $targetVersion) {
        $same = $installedVersion -eq $targetVersion
        $newer = ($Version -eq 'latest') -and ($installedVersion -gt $targetVersion)
        if ($same -or $newer) {
            if ($same) {
                Write-Ok (T 'already installed, nothing to do (-Force installs it again)' 'já está instalada, nada a fazer (-Force instala de novo)')
            } else {
                Write-Ok (T 'the installed version is newer than the published one (-Force installs it anyway)' 'a versão instalada é mais nova que a publicada (-Force instala mesmo assim)')
            }
            Install-Self
            Install-Shortcuts
            $running = (Get-RustInputProcess).Count -gt 0
            if (-not $running -and -not $NoStart) {
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

    Write-Step ((T 'Downloading {0}' 'Baixando {0}') -f $AssetName)
    $zip = Join-Path $env:TEMP ("rustinput-{0}.zip" -f $target)
    $download = Save-File $asset.browser_download_url $zip ([long]$asset.size)
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
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    Remove-Folder $StagingDir
    [System.IO.Compression.ZipFile]::ExtractToDirectory($zip, $StagingDir)
    $stagedExe = Join-Path $StagingDir 'rustinput.exe'
    if (-not (Test-Path -LiteralPath $stagedExe)) {
        Remove-Folder $StagingDir
        throw ((T '{0} has no rustinput.exe' 'o {0} não tem o rustinput.exe') -f $AssetName)
    }
    $files = @(Get-ChildItem -LiteralPath $StagingDir -Recurse -File)
    $size = ($files | Measure-Object -Property Length -Sum).Sum
    Write-Ok ((T 'unpacked: {0} files, {1}' 'descompactado: {0} arquivos, {1}') -f $files.Count, (Format-Size $size))

    Write-Step (T 'Closing RustInput' 'Fechando o RustInput')
    Stop-RustInput

    Write-Step ((T 'Installing {0}' 'Instalando a {0}') -f $target)
    Install-Staged
    Remove-Item -LiteralPath $zip -Force -ErrorAction SilentlyContinue
    Install-Self
    $now = Get-ExeVersion $Exe
    if ($now) {
        Write-Ok ((T 'installed: rustinput.exe {0}' 'instalado: rustinput.exe {0}') -f $now)
    } else {
        Write-Ok ((T 'installed in {0}' 'instalado em {0}') -f $InstallDir)
    }
    Install-Shortcuts

    Write-Step (T 'Starting RustInput' 'Abrindo o RustInput')
    $running = $false
    if ($NoStart) {
        Write-Info (T 'skipped (-NoStart)' 'pulado (-NoStart)')
    } else {
        $running = Start-RustInput
        if (-not $running -and (Test-Path -LiteralPath $PreviousDir)) {
            Write-Warn (T 'to go back to the previous version, run this again with -Restore' 'para voltar para a versão anterior, rode de novo com -Restore')
        }
    }
    Show-Summary (Get-VersionText $installed) $target $running
}

# ---------------------------------------------------------------------------

try {
    New-Item -ItemType Directory -Force -Path $LogDir | Out-Null
    Start-Transcript -Path $UpdateLog -Append | Out-Null
    $script:TranscriptOn = $true
} catch { }

try {
    # Windows PowerShell 5.1 may still default to older TLS versions
    [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12
} catch { }

Write-Host ''
Write-Host '  RustInput  ' -ForegroundColor Black -BackgroundColor DarkYellow -NoNewline
Write-Host (T '  install and update for Windows' '  instalação e atualização para Windows')
Write-Detail ('PowerShell {0}, {1}' -f $PSVersionTable.PSVersion, (Get-Date).ToString('g'))

try {
    if ($Restore) {
        Invoke-Restore
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
