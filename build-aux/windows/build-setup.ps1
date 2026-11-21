# Builds the installer (rustinput-windows-x86_64-setup.exe) from a folder that
# holds what the zip of a release holds, with Inno Setup 6, which it installs
# with Chocolatey when it is missing (GitHub runners have Chocolatey). The
# version is that of rustinput.exe, which build.rs takes from Cargo.toml.
#
#   powershell -File build-aux\windows\build-setup.ps1 -PackageDir rustinput-windows -OutputDir out
param(
    [Parameter(Mandatory = $true)][string]$PackageDir,
    [string]$OutputDir = '.'
)
$ErrorActionPreference = 'Stop'

function Find-Iscc {
    $roots = @(${env:ProgramFiles(x86)}, $env:ProgramFiles) | Where-Object { $_ }
    return Get-ChildItem -Path ($roots | ForEach-Object { Join-Path $_ 'Inno Setup *\ISCC.exe' }) -ErrorAction SilentlyContinue |
        Sort-Object FullName | Select-Object -Last 1
}

$iscc = Find-Iscc
if (-not $iscc) {
    choco install innosetup --no-progress -y | Out-Host
    $iscc = Find-Iscc
}
if (-not $iscc) { throw 'Inno Setup is not installed (no ISCC.exe)' }
Write-Host ('Inno Setup: {0} {1}' -f $iscc.FullName, $iscc.VersionInfo.ProductVersion)

$package = (Resolve-Path -LiteralPath $PackageDir).Path
$exe = Join-Path $package 'rustinput.exe'
$version = ([string](Get-Item -LiteralPath $exe).VersionInfo.ProductVersion).Trim()
if ($version -notmatch '^\d+\.\d+\.\d+$') { throw "the version of rustinput.exe is not x.y.z: '$version'" }
New-Item -ItemType Directory -Force -Path $OutputDir | Out-Null
$output = (Resolve-Path -LiteralPath $OutputDir).Path

& $iscc.FullName /Qp "/DAppVersion=$version" "/DPackageDir=$package" "/DOutputDir=$output" (Join-Path $PSScriptRoot 'rustinput.iss')
if ($LASTEXITCODE -ne 0) { throw "ISCC failed with $LASTEXITCODE" }
$setup = Join-Path $output 'rustinput-windows-x86_64-setup.exe'
Write-Host ('{0}: {1:N1} MB, SHA-256 {2}' -f $setup, ((Get-Item -LiteralPath $setup).Length / 1MB), (Get-FileHash -Algorithm SHA256 -LiteralPath $setup).Hash.ToLowerInvariant())
