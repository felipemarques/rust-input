# Installs this build with update-rustinput.ps1, from a zip as a release would
# bring it, checks that the service starts the daemon in the session at the
# screen with administrator rights and starts it again when it ends, and
# removes it all again. Run by .github/workflows/windows-service.yml, as
# administrator, after `cargo build --release`.
$ErrorActionPreference = 'Stop'
$script:Failed = $false
function Fail([string]$Text) {
    Write-Host "::error::$Text"
    $script:Failed = $true
}

function Get-Daemon {
    return @(Get-CimInstance -ClassName Win32_Process -Filter "Name='rustinput.exe'" |
        Where-Object { [string]$_.CommandLine -match '\sdaemon(\s|$)' })
}

$updater = Join-Path $PSScriptRoot '..\scripts\update-rustinput.ps1'
$exe = Join-Path $PSScriptRoot '..\target\release\rustinput.exe'

Write-Host '== who runs this, and who is at the screen'
whoami
whoami /groups | Select-String 'Mandatory Label'
cmd /c 'query user 2>&1'
cmd /c 'qwinsta 2>&1'

$package = Join-Path $env:RUNNER_TEMP 'package'
New-Item -ItemType Directory -Force -Path $package | Out-Null
Copy-Item -LiteralPath $exe, $updater -Destination $package
$zip = Join-Path $env:RUNNER_TEMP 'rustinput-windows-x86_64.zip'
Compress-Archive -Path (Join-Path $package '*') -DestinationPath $zip -Force

Write-Host '== install'
& powershell -NoProfile -ExecutionPolicy Bypass -File $updater -Zip $zip -NoStart -NoShortcuts -NoPause
if ($LASTEXITCODE -ne 0) { Fail "the installation failed (exit code $LASTEXITCODE)" }

$service = Get-CimInstance -ClassName Win32_Service -Filter "Name='RustInput'"
if (-not $service) {
    Fail 'the service is not installed'
} else {
    Write-Host ('service: {0}, {1}, {2}, as {3}' -f $service.State, $service.StartMode, $service.PathName, $service.StartName)
    if ($service.State -ne 'Running') { Fail 'the service does not run' }
}
cmd /c 'sc.exe qfailure RustInput 2>&1'
Get-NetFirewallRule -Group RustInput -ErrorAction SilentlyContinue | Format-Table DisplayName, Profile, Enabled, Action -AutoSize | Out-Host

$daemon = Get-Daemon
if ($daemon.Count -eq 0) {
    Write-Host '::warning::no daemon: is anybody logged on at the screen of this runner?'
} else {
    $owner = Invoke-CimMethod -InputObject $daemon[0] -MethodName GetOwner
    Write-Host ('daemon: PID {0}, session {1}, user {2}\{3}' -f $daemon[0].ProcessId, $daemon[0].SessionId, $owner.Domain, $owner.User)
    $log = "C:\Users\$($owner.User)\AppData\Local\rustinput\logs\daemon.log"
    Start-Sleep -Seconds 3
    if (Test-Path -LiteralPath $log) {
        Write-Host "== $log"
        Get-Content -LiteralPath $log | Out-Host
        if (-not (Select-String -LiteralPath $log -Pattern 'daemon, with administrator rights' -Quiet)) {
            Fail 'the daemon does not run with administrator rights'
        }
    } else {
        Write-Host "::warning::no $log"
    }

    Write-Host '== the service starts the daemon again when it ends'
    Stop-Process -Id $daemon[0].ProcessId -Force
    $deadline = (Get-Date).AddSeconds(15)
    $again = @()
    while ($again.Count -eq 0 -and (Get-Date) -lt $deadline) {
        Start-Sleep -Milliseconds 500
        $again = @(Get-Daemon | Where-Object { $_.ProcessId -ne $daemon[0].ProcessId })
    }
    if ($again.Count -eq 0) {
        Fail 'the service did not start the daemon again'
    } else {
        Write-Host ('started again: PID {0}' -f $again[0].ProcessId)
    }
}

$serviceLog = Join-Path $env:SystemRoot 'Logs\RustInput\service.log'
Write-Host "== $serviceLog"
if (Test-Path -LiteralPath $serviceLog) {
    Get-Content -LiteralPath $serviceLog | Out-Host
    # users may read the log and not write there
    icacls (Split-Path -Parent $serviceLog) | Out-Host
} else {
    Fail 'the service wrote no log'
}

Write-Host '== uninstall'
& powershell -NoProfile -ExecutionPolicy Bypass -File $updater -Uninstall -NoPause
if ($LASTEXITCODE -ne 0) { Fail "the removal failed (exit code $LASTEXITCODE)" }
if (Get-Service -Name RustInput -ErrorAction SilentlyContinue) { Fail 'the service is still installed' }
if (Test-Path -LiteralPath (Join-Path $env:ProgramFiles 'RustInput')) { Fail 'the folder is still there' }
if (Get-Process -Name rustinput -ErrorAction SilentlyContinue) { Fail 'RustInput still runs' }
if (@(Get-NetFirewallRule -Group RustInput -ErrorAction SilentlyContinue).Count -gt 0) { Fail 'the firewall rules are still there' }

if ($script:Failed) { exit 1 }
Write-Host 'all good'
