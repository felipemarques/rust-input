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
#   7. forward again, then the uninstaller of Settings > Apps
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
    $log = "C:\Users\$($owner.User)\AppData\Local\rustinput\logs\daemon.log"
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
    Stop-Process -Id $daemon[0].ProcessId -Force
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

Write-Stage '7. forward again, then the uninstaller of Settings > Apps'
Invoke-Updater @('-Setup', $setupPath)
Test-Installed '7' $version $true
$settings = @(Get-Daemon | ForEach-Object { (Invoke-CimMethod -InputObject $_ -MethodName GetOwner).User } |
    ForEach-Object { "C:\Users\$_\AppData\Local\rustinput" } | Where-Object { Test-Path -LiteralPath $_ })
$uninstaller = ([string](Get-ItemProperty -LiteralPath $key).UninstallString).Trim('"')
Start-Process -FilePath $uninstaller -ArgumentList '/VERYSILENT', '/SUPPRESSMSGBOXES', '/NORESTART' -Wait
$deadline = (Get-Date).AddSeconds(120)
while (((Test-Path -LiteralPath $key) -or (Test-Path -LiteralPath $uninstaller)) -and (Get-Date) -lt $deadline) { Start-Sleep -Milliseconds 500 }
Test-Exists '7' $key $false
if (Get-Service -Name RustInput -ErrorAction SilentlyContinue) { Fail '7: the service is still installed' }
if (Get-Process -Name rustinput -ErrorAction SilentlyContinue) { Fail '7: RustInput still runs' }
if (@(Get-NetFirewallRule -Group RustInput -ErrorAction SilentlyContinue).Count -gt 0) { Fail '7: the firewall rules are still there' }
Test-Exists '7' $folder $false
Test-Exists '7' $previous $false
Test-Exists '7' $logs $false
Test-Exists '7' (Join-Path $commonPrograms 'RustInput.lnk') $false
Test-Exists '7' (Join-Path $commonStartup 'RustInput.lnk') $false
foreach ($dir in $settings) { Test-Exists '7, the settings stay' $dir $true }

Write-Stage '8. update-rustinput.ps1 -Uninstall with nothing installed'
Invoke-Updater @('-Uninstall')

if ($script:Failed) { exit 1 }
Write-Host 'all good'
