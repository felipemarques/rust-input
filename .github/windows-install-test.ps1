# Installs RustInput on a Windows runner the ways a user would, and checks
# each step. Run by .github/workflows/windows-install.yml, as administrator,
# after building this version and its installer:
#   1. 0.23.1 from GitHub with update-rustinput.ps1 (its zip), as before the
#      installer; the firewall rules then also allow public networks, as a
#      user may have chosen
#   2. this installer over it, without questions: it takes that over
#   3. the installer again
#   4. update-rustinput.ps1 -Version 0.23.1: back to the zip
#   5. update-rustinput.ps1 -Setup: forward with the installer
#   6. update-rustinput.ps1 -Restore: back to the copy kept of 0.23.1
#   7. forward again, then the uninstaller of Settings > Apps; before it,
#      the lock screen (0.31.0): the daemon runs as SYSTEM for the
#      administrator at the screen, as that user with login_screen = false,
#      and follows the input to the lock screen once it is locked; and
#      (0.32.1) only the user at the screen talks to it, on the named pipe
#      of the session, and a junction in the folders of the user does not
#      make it delete elsewhere
#   8. update-rustinput.ps1 -Uninstall with nothing installed
param(
    [Parameter(Mandatory = $true)][string]$Setup
)
$ErrorActionPreference = 'Stop'
$script:Failed = $false
function Fail([string]$Text) {
    Write-Host "::error::$Text"
    $script:Failed = $true
}
function Write-Stage([string]$Text) {
    Write-Host ''
    Write-Host "===== $Text" -ForegroundColor Cyan
}

$updater = (Resolve-Path -LiteralPath (Join-Path $PSScriptRoot '..\scripts\update-rustinput.ps1')).Path
$setupPath = (Resolve-Path -LiteralPath $Setup).Path
$version = ([string](Get-Item -LiteralPath $setupPath).VersionInfo.ProductVersion).Trim()
$old = '0.23.1'
$folder = Join-Path $env:ProgramFiles 'RustInput'
$exe = Join-Path $folder 'rustinput.exe'
$previous = "$folder.previous"
$key = 'HKLM:\SOFTWARE\Microsoft\Windows\CurrentVersion\Uninstall\{8C7A4E2B-3F1D-4B6A-9E5C-2D7F1A0B6C34}_is1'
$userPrograms = [Environment]::GetFolderPath('Programs')
$commonPrograms = [Environment]::GetFolderPath('CommonPrograms')
$commonStartup = [Environment]::GetFolderPath('CommonStartup')
$logs = Join-Path $env:SystemRoot 'Logs\RustInput'
# the runner is the user at the screen: the daemon uses its folders
$config = Join-Path $env:LOCALAPPDATA 'rustinput\config.toml'
$daemonLog = Join-Path $env:LOCALAPPDATA 'rustinput\logs\daemon.log'
$remembered = 'HKLM:\SOFTWARE\RustInput'

Write-Host "this build: $version, from $setupPath"
whoami
cmd /c 'query user 2>&1'

function Get-Daemon {
    return @(Get-CimInstance -ClassName Win32_Process -Filter "Name='rustinput.exe'" |
        Where-Object { [string]$_.CommandLine -match '\sdaemon(\s|$)' })
}

function Get-Version([string]$Path) {
    if (-not (Test-Path -LiteralPath $Path)) { return '' }
    return ([string](Get-Item -LiteralPath $Path).VersionInfo.ProductVersion).Trim()
}

function Invoke-Updater([string[]]$Arguments) {
    Write-Host "update-rustinput.ps1 $($Arguments -join ' ')"
    & powershell -NoProfile -ExecutionPolicy Bypass -File $updater @Arguments -NoStart -NoPause
    if ($LASTEXITCODE -ne 0) { Fail "update-rustinput.ps1 $($Arguments -join ' ') ended with $LASTEXITCODE" }
}

function Invoke-Installer([string]$Label) {
    $log = Join-Path $env:RUNNER_TEMP "setup-$Label.log"
    $process = Start-Process -FilePath $setupPath -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', '/SP-', ('/LOG="{0}"' -f $log) -Wait -PassThru
    if ($process.ExitCode -ne 0) {
        Get-Content -LiteralPath $log -Tail 40 -ErrorAction SilentlyContinue | Out-Host
        Fail "${Label}: the installer ended with $($process.ExitCode)"
    }
}

# what each installation must have: the version, Settings > Apps, the
# service, how Windows starts it again, and two firewall rules for the
# installed rustinput.exe that kept the public networks
function Test-Installed([string]$Label, [string]$Expected, [bool]$BySetup, [bool]$Public = $true) {
    $now = Get-Version $exe
    if ($now -ne $Expected) { Fail "${Label}: rustinput.exe is '$now', not $Expected" }
    $listed = Test-Path -LiteralPath $key
    if ($listed -ne $BySetup) { Fail "${Label}: listed in Settings > Apps: $listed, expected $BySetup" }
    if ($listed) {
        $shown = (Get-ItemProperty -LiteralPath $key).DisplayVersion
        if ($shown -ne $Expected) { Fail "${Label}: Settings > Apps shows $shown" }
    }
    $service = Get-CimInstance -ClassName Win32_Service -Filter "Name='RustInput'"
    if (-not $service) {
        Fail "${Label}: the service is not installed"
    } else {
        Write-Host ('service: {0}, {1}, {2}, as {3}' -f $service.State, $service.StartMode, $service.PathName, $service.StartName)
        if ($service.State -ne 'Running') { Fail "${Label}: the service does not run" }
        if ($service.StartMode -ne 'Auto') { Fail "${Label}: the service does not start with Windows" }
        if ($service.PathName -notlike ('"{0}"*windows-service' -f $exe)) { Fail "${Label}: the service runs $($service.PathName)" }
    }
    $failure = (cmd /c 'sc.exe qfailure RustInput 2>&1') -join "`n"
    if ($failure -notmatch 'RESTART') { Fail "${Label}: Windows does not start the service again when it fails:`n$failure" }
    $rules = @(Get-NetFirewallRule -Group RustInput -ErrorAction SilentlyContinue)
    $rules | Format-Table DisplayName, Profile, Enabled, Action -AutoSize | Out-Host
    if ($rules.Count -ne 2) { Fail "${Label}: $($rules.Count) firewall rules instead of 2" }
    foreach ($rule in $rules) {
        $program = ($rule | Get-NetFirewallApplicationFilter).Program
        if ($program -ne $exe) { Fail "${Label}: the rule $($rule.DisplayName) is for $program" }
        if ($Public -and [string]$rule.Profile -notmatch 'Public|Any') { Fail "${Label}: the rule $($rule.DisplayName) lost the public networks ($($rule.Profile))" }
    }
}

# the service starts the daemon of `Expected` in the session at the screen,
# with administrator rights, and again when it ends
function Test-Daemon([string]$Label, [string]$Expected) {
    $deadline = (Get-Date).AddSeconds(30)
    $daemon = @()
    while ($daemon.Count -eq 0 -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 500
        $daemon = Get-Daemon
    }
    if ($daemon.Count -eq 0) {
        Write-Host "::warning::${Label}: no daemon: is anybody logged on at the screen of this runner?"
        return
    }
    $owner = Invoke-CimMethod -InputObject $daemon[0] -MethodName GetOwner
    Write-Host ('daemon: PID {0}, session {1}, user {2}\{3}, {4}' -f $daemon[0].ProcessId, $daemon[0].SessionId, $owner.Domain, $owner.User, $daemon[0].ExecutablePath)
    if ($daemon[0].ExecutablePath -ne $exe) { Fail "${Label}: the daemon runs $($daemon[0].ExecutablePath)" }
    # for an administrator, as SYSTEM since 0.31.0, with the folders of the user
    $user = if ([version]$Expected -ge [version]'0.31.0') { 'SYSTEM' } else { $env:USERNAME }
    if ($owner.User -ne $user) { Fail "${Label}: the daemon runs as $($owner.User), not $user" }
    $log = $daemonLog
    $line = "RustInput $Expected daemon, with administrator rights"
    $deadline = (Get-Date).AddSeconds(15)
    $found = $false
    while (-not $found -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 500
        $found = (Test-Path -LiteralPath $log) -and (Select-String -LiteralPath $log -SimpleMatch $line -Quiet)
    }
    if (-not $found) {
        Get-Content -LiteralPath $log -ErrorAction SilentlyContinue | Out-Host
        Fail "${Label}: no '$line' in $log"
    }
    # taskkill: a daemon of SYSTEM takes the right to debug, which it turns on
    cmd /c "taskkill.exe /F /PID $($daemon[0].ProcessId) 2>&1" | Out-Host
    $deadline = (Get-Date).AddSeconds(15)
    $again = @()
    while ($again.Count -eq 0 -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 500
        $again = @(Get-Daemon | Where-Object { $_.ProcessId -ne $daemon[0].ProcessId })
    }
    if ($again.Count -eq 0) { Fail "${Label}: the service did not start the daemon again" } else { Write-Host "started again: PID $($again[0].ProcessId)" }
}

function Test-Exists([string]$Label, [string]$Path, [bool]$Expected) {
    $there = Test-Path -LiteralPath $Path
    if ($there -ne $Expected) { Fail "${Label}: $Path there: $there, expected $Expected" }
}

Write-Stage "1. $old with update-rustinput.ps1"
Invoke-Updater @('-Version', $old)
Test-Installed '1' $old $false $false
Test-Exists '1' (Join-Path $userPrograms 'RustInput.lnk') $true
# as if the user had allowed public networks when Windows asked
Get-NetFirewallRule -Group RustInput | Set-NetFirewallRule -Profile Domain, Private, Public

Write-Stage "2. the installer of $version over it"
Invoke-Installer '2'
Test-Installed '2' $version $true
Test-Exists '2' (Join-Path $folder 'unins000.exe') $true
Test-Exists '2' (Join-Path $userPrograms 'RustInput.lnk') $false
Test-Exists '2' (Join-Path $commonPrograms 'RustInput.lnk') $true
Test-Exists '2' (Join-Path $commonPrograms 'Update RustInput.lnk') $true
Test-Exists '2' (Join-Path $commonStartup 'RustInput.lnk') $true
Test-Exists '2' (Join-Path $logs 'setup.log') $true
Get-Content -LiteralPath (Join-Path $logs 'setup.log') -ErrorAction SilentlyContinue | Out-Host
Test-Daemon '2' $version

Write-Stage '3. the installer again'
Invoke-Installer '3'
Test-Installed '3' $version $true

Write-Stage "4. back to $old with update-rustinput.ps1 -Version $old"
Invoke-Updater @('-Version', $old)
Test-Installed '4' $old $false
Test-Exists '4' (Join-Path $folder 'unins000.exe') $false
Test-Exists '4' (Join-Path $commonPrograms 'RustInput.lnk') $false
Test-Exists '4' (Join-Path $userPrograms 'RustInput.lnk') $true
$kept = Get-Version (Join-Path $previous 'rustinput.exe')
if ($kept -ne $version) { Fail "4: the copy kept in $previous is '$kept', not $version" }

Write-Stage "5. forward with update-rustinput.ps1 -Setup"
Invoke-Updater @('-Setup', $setupPath)
Test-Installed '5' $version $true
$kept = Get-Version (Join-Path $previous 'rustinput.exe')
if ($kept -ne $old) { Fail "5: the copy kept in $previous is '$kept', not $old" }

Write-Stage '6. update-rustinput.ps1 -Restore'
Invoke-Updater @('-Restore')
Test-Installed '6' $old $false
$kept = Get-Version (Join-Path $previous 'rustinput.exe')
if ($kept -ne $version) { Fail "6: the copy kept in $previous is '$kept', not $version" }

# the daemon runs as `User` within `Seconds`
function Wait-DaemonOwner([string]$Label, [string]$User, [int]$Seconds = 30) {
    $deadline = (Get-Date).AddSeconds($Seconds)
    do {
        Start-Sleep -Milliseconds 500
        # a daemon that ends meanwhile (the service restarting it) is no owner
        $owners = @(Get-Daemon | ForEach-Object {
            try { (Invoke-CimMethod -InputObject $_ -MethodName GetOwner -ErrorAction Stop).User } catch { }
        })
    } while (($owners.Count -ne 1 -or $owners[0] -ne $User) -and (Get-Date) -lt $deadline)
    if ($owners.Count -ne 1 -or $owners[0] -ne $User) {
        Fail "${Label}: the daemon runs as '$($owners -join ', ')', not $User"
    } else {
        Write-Host "  [ok] the daemon runs as $User"
    }
}

# what the service remembers for the screen to log on
function Get-Remembered {
    return [string](Get-ItemProperty -LiteralPath $remembered -Name LoginScreenLocalAppData -ErrorAction SilentlyContinue).LoginScreenLocalAppData
}

Write-Stage '7. forward again, then the uninstaller of Settings > Apps'
Invoke-Updater @('-Setup', $setupPath)
Test-Installed '7' $version $true

Write-Stage '7a. the lock screen: as SYSTEM, as the user with login_screen = false'
Wait-DaemonOwner '7a' 'SYSTEM'
if ((Get-Remembered) -ne $env:LOCALAPPDATA) { Fail "7a: remembered for the screen to log on: '$(Get-Remembered)', not $env:LOCALAPPDATA" }
$before = if (Test-Path -LiteralPath $config) { Get-Content -LiteralPath $config -Raw } else { '' }
# before any table of the file, in UTF-8 without the mark PowerShell 5 puts
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $config) | Out-Null
[System.IO.File]::WriteAllText($config, "login_screen = false`r`n" + $before)
Wait-DaemonOwner '7a, login_screen = false' $env:USERNAME
if (Get-Remembered) { Fail "7a: with login_screen = false the service still remembers '$(Get-Remembered)'" }
if ((Get-Content -LiteralPath $config -Raw) -notmatch '(?m)^login_screen = false') { Fail '7a: login_screen = false left the configuration' }
[System.IO.File]::WriteAllText($config, $before)
Wait-DaemonOwner '7a, login_screen again' 'SYSTEM'

Write-Stage '7d. only the user at the screen talks to the daemon (0.32.1)'
$session = (Get-Daemon | Select-Object -First 1).SessionId
$pipe = "rustinput-$session"
if (@(Get-NetTCPConnection -LocalPort 5252 -State Listen -ErrorAction SilentlyContinue).Count -gt 0) {
    Fail '7d: something listens on TCP 5252, the port of the window before 0.32.1'
}
if (@([System.IO.Directory]::GetFiles('\\.\pipe\') | Where-Object { $_ -ieq "\\.\pipe\$pipe" }).Count -ne 1) {
    Fail "7d: no named pipe \\.\pipe\$pipe"
}
# opens the pipe like a window would and tells what the daemon sent first
$probe = Join-Path $env:PUBLIC 'rustinput-probe.ps1'
Set-Content -LiteralPath $probe -Encoding ASCII -Value @'
param([string]$Pipe, [string]$Out)
try {
    $client = New-Object System.IO.Pipes.NamedPipeClientStream('.', $Pipe, [System.IO.Pipes.PipeDirection]::InOut, [System.IO.Pipes.PipeOptions]::None, [System.Security.Principal.TokenImpersonationLevel]::Identification)
    $client.Connect(5000)
    $reader = New-Object System.IO.StreamReader($client)
    $line = $reader.ReadLineAsync()
    if ($line.Wait(5000) -and $line.Result) {
        $text = 'answered: ' + $line.Result.Substring(0, [Math]::Min(50, $line.Result.Length))
    } else {
        $text = 'no answer'
    }
} catch {
    $text = 'refused: ' + $_.Exception.Message
}
Set-Content -LiteralPath $Out -Value $text
'@
$mine = Join-Path $env:PUBLIC 'rustinput-probe-mine.txt'
& powershell.exe -NoProfile -ExecutionPolicy Bypass -File $probe -Pipe $pipe -Out $mine
$answer = Get-Content -LiteralPath $mine -Raw -ErrorAction SilentlyContinue
Write-Host "  the user at the screen: $answer"
if ($answer -notmatch '^answered: \{') { Fail "7d: the user at the screen got no answer from the daemon: $answer" }
# another user of this computer, in this session
$other = 'rustinputprobe'
# 14 characters at most: net user asks about longer ones
$password = 'Ri!' + [guid]::NewGuid().ToString('N').Substring(0, 11)
cmd /c "net user $other $password /add 2>&1" | Out-Host
$theirs = Join-Path $env:PUBLIC 'rustinput-probe-theirs.txt'
Remove-Item -LiteralPath $theirs -ErrorAction SilentlyContinue
try {
    $credential = New-Object System.Management.Automation.PSCredential($other, (ConvertTo-SecureString $password -AsPlainText -Force))
    Start-Process -FilePath powershell.exe -Credential $credential -WorkingDirectory $env:PUBLIC -WindowStyle Hidden -Wait `
        -ArgumentList @('-NoProfile', '-ExecutionPolicy', 'Bypass', '-File', $probe, '-Pipe', $pipe, '-Out', $theirs)
    $answer = Get-Content -LiteralPath $theirs -Raw -ErrorAction SilentlyContinue
    Write-Host "  another user: $answer"
    if (-not $answer) {
        Fail '7d: the process of another user did not run'
    } elseif ($answer -match '^answered') {
        Fail "7d: the daemon answered another user: $answer"
    }
    $refused = Select-String -LiteralPath $daemonLog -SimpleMatch 'refused a connection: it runs as another user' -Quiet
    if ($answer -notmatch '^refused' -and -not $refused) { Fail "7d: no 'refused a connection' in $daemonLog" }
} catch {
    Fail "7d: could not run a process as another user: $($_.Exception.Message)"
} finally {
    cmd /c "net user $other /delete 2>&1" | Out-Host
}

Write-Stage '7e. a junction in the folders of the user: SYSTEM removes nothing elsewhere'
$victim = Join-Path $env:SystemDrive 'rustinput-victim'
New-Item -ItemType Directory -Force -Path (Join-Path $victim '000000000000001-1') | Out-Null
$received = Join-Path $env:LOCALAPPDATA 'rustinput\received'
Remove-Item -LiteralPath $received -Recurse -Force -ErrorAction SilentlyContinue
New-Item -ItemType Directory -Force -Path (Split-Path -Parent $received) | Out-Null
cmd /c "mklink /J `"$received`" `"$victim`" 2>&1" | Out-Host
# a new daemon removes the transfers of the one before
$daemon = @(Get-Daemon)
cmd /c "taskkill.exe /F /PID $($daemon[0].ProcessId) 2>&1" | Out-Host
$deadline = (Get-Date).AddSeconds(20)
do {
    Start-Sleep -Milliseconds 500
    $again = @(Get-Daemon | Where-Object { $_.ProcessId -ne $daemon[0].ProcessId })
} while ($again.Count -eq 0 -and (Get-Date) -lt $deadline)
$noted = $false
$deadline = (Get-Date).AddSeconds(15)
while (-not $noted -and (Get-Date) -lt $deadline) {
    Start-Sleep -Milliseconds 500
    $noted = (Test-Path -LiteralPath $daemonLog) -and (Select-String -LiteralPath $daemonLog -SimpleMatch 'is a link: nothing is removed' -Quiet)
}
if ($again.Count -eq 0) { Fail '7e: the service did not start the daemon again' }
if (-not (Test-Path -LiteralPath (Join-Path $victim '000000000000001-1'))) {
    Fail '7e: the daemon removed a folder through the junction'
} else {
    Write-Host '  [ok] the folder behind the junction is still there'
}
if (-not $noted) { Fail "7e: no 'is a link: nothing is removed' in $daemonLog" }
$guard = Select-String -LiteralPath $daemonLog -Pattern 'Redirection Guard' | Select-Object -Last 1
Write-Host "  $($guard.Line)"
cmd /c "rmdir `"$received`" 2>&1" | Out-Host

Write-Stage '7b. locked: the daemon follows the input to the lock screen'
$line = 'following the input to the desktop Winlogon of Windows'
rundll32.exe user32.dll,LockWorkStation
$deadline = (Get-Date).AddSeconds(20)
$found = $false
while (-not $found -and (Get-Date) -lt $deadline) {
    Start-Sleep -Milliseconds 500
    $found = (Test-Path -LiteralPath $daemonLog) -and (Select-String -LiteralPath $daemonLog -SimpleMatch $line -Quiet)
}
Get-Content -LiteralPath $daemonLog -Tail 15 -ErrorAction SilentlyContinue | Out-Host
if ($found) {
    Write-Host "  [ok] $((Select-String -LiteralPath $daemonLog -SimpleMatch $line | Select-Object -Last 1).Line)"
} else {
    Fail "7b: no '$line' in $daemonLog"
}

Write-Stage '7c. the uninstaller of Settings > Apps'
$settings = @(Join-Path $env:LOCALAPPDATA 'rustinput') | Where-Object { Test-Path -LiteralPath $_ }
$uninstaller = ([string](Get-ItemProperty -LiteralPath $key).UninstallString).Trim('"')
$uninstallLog = Join-Path $env:RUNNER_TEMP 'uninstall.log'
Start-Process -FilePath $uninstaller -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART', ('/LOG="{0}"' -f $uninstallLog) -Wait
# it goes on in a copy of itself in %TEMP% (_unins.tmp, or _iu*.tmp with an older Inno Setup)
$deadline = (Get-Date).AddSeconds(120)
while (((Test-Path -LiteralPath $key) -or (Test-Path -LiteralPath $uninstaller) -or
        @(Get-Process | Where-Object { $_.Name -like '_iu*' -or $_.Name -like '_unins*' }).Count -gt 0) -and (Get-Date) -lt $deadline) {
    Start-Sleep -Milliseconds 500
}
Write-Host "== $uninstallLog"
Get-Content -LiteralPath $uninstallLog -ErrorAction SilentlyContinue | Out-Host
foreach ($path in (Join-Path $logs 'setup.log'), (Join-Path $logs 'service.log')) {
    if (Test-Path -LiteralPath $path) {
        Write-Host "== $path"
        Get-Content -LiteralPath $path | Out-Host
    }
}
if (Test-Path -LiteralPath $folder) {
    Write-Host "== left in $folder"
    Get-ChildItem -LiteralPath $folder -Recurse -Force | Select-Object FullName, Length | Out-Host
}
Test-Exists '7' $key $false
if (Get-Service -Name RustInput -ErrorAction SilentlyContinue) { Fail '7: the service is still installed' }
if (Get-Process -Name rustinput -ErrorAction SilentlyContinue) { Fail '7: RustInput still runs' }
if (@(Get-NetFirewallRule -Group RustInput -ErrorAction SilentlyContinue).Count -gt 0) { Fail '7: the firewall rules are still there' }
Test-Exists '7' $folder $false
Test-Exists '7' $previous $false
Test-Exists '7' $logs $false
Test-Exists '7' (Join-Path $commonPrograms 'RustInput.lnk') $false
Test-Exists '7' (Join-Path $commonStartup 'RustInput.lnk') $false
Test-Exists '7' $remembered $false
foreach ($dir in $settings) { Test-Exists '7, the settings stay' $dir $true }

Write-Stage '8. update-rustinput.ps1 -Uninstall with nothing installed'
Invoke-Updater @('-Uninstall')

if ($script:Failed) { exit 1 }
Write-Host 'all good'
