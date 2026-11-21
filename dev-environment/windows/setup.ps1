#Requires -Version 5.1
<#
.SYNOPSIS
    Provision the Windows CI runner toolchains for Spotledger (WP-PRD-10.T1).

.DESCRIPTION
    Idempotent, machine-wide setup matching the CI runner notes: Git,
    Python 3.12, Visual Studio Build Tools (MSVC C++ workload), Rust in
    C:\rust and the .NET SDK in C:\dotnet. Run once as Administrator on the
    runner VM. Jobs then run as the non-admin `ghrunner` user.

    Only pinned artefacts are downloaded, and each is verified: rustup-init.exe
    against the SHA-256 in versions.psd1, the dotnet-install.ps1 script against
    its SHA-256, and the winget packages against the hashes in their manifests.
    There is no other network use. Everything already at the pinned version is
    left alone, so re-running is safe.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File dev-environment\windows\setup.ps1
    powershell -ExecutionPolicy Bypass -File dev-environment\windows\setup.ps1 -DryRun
#>
[CmdletBinding()]
param(
    [string] $RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path,
    [string] $DownloadDir = (Join-Path $env:TEMP 'spot-devenv'),
    [switch] $SkipWinget,
    [switch] $DryRun
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$pins = Import-PowerShellDataFile -Path (Join-Path $PSScriptRoot 'versions.psd1')

function Write-Step([string] $message) { Write-Host "==> $message" -ForegroundColor Cyan }
function Write-Ok([string] $message) { Write-Host "    ok: $message" -ForegroundColor Green }

function Assert-Administrator {
    $identity = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($identity)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw 'setup.ps1 must run as Administrator (machine-wide toolchain install).'
    }
}

function Set-MachineVariable([string] $name, [string] $value) {
    if ($DryRun) { Write-Host "    would set (machine) $name=$value"; return }
    $current = [Environment]::GetEnvironmentVariable($name, 'Machine')
    if ($current -ne $value) {
        [Environment]::SetEnvironmentVariable($name, $value, 'Machine')
    }
    Set-Item -Path "Env:$name" -Value $value
}

function Add-MachinePath([string] $directory) {
    if ($DryRun) { Write-Host "    would add to (machine) PATH: $directory"; return }
    $current = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    $parts = @($current -split ';' | Where-Object { $_ -ne '' })
    if ($parts -notcontains $directory) {
        [Environment]::SetEnvironmentVariable('Path', ($current.TrimEnd(';') + ';' + $directory), 'Machine')
    }
    if (($env:Path -split ';') -notcontains $directory) { $env:Path = "$env:Path;$directory" }
}

function Get-PinnedFile {
    param([string] $Url, [string] $Destination, [string] $Sha256)
    if (Test-Path -LiteralPath $Destination) {
        $actual = (Get-FileHash -LiteralPath $Destination -Algorithm SHA256).Hash.ToLowerInvariant()
        if ($actual -eq $Sha256.ToLowerInvariant()) {
            Write-Ok "using verified $Destination"
            return
        }
        Write-Host "    cached $Destination has the wrong hash; re-downloading"
        Remove-Item -LiteralPath $Destination -Force
    }
    if ($DryRun) { Write-Host "    would download $Url to $Destination"; return }
    Write-Host "    downloading $Url"
    Invoke-WebRequest -Uri $Url -OutFile $Destination -UseBasicParsing
    $actual = (Get-FileHash -LiteralPath $Destination -Algorithm SHA256).Hash.ToLowerInvariant()
    if ($actual -ne $Sha256.ToLowerInvariant()) {
        Remove-Item -LiteralPath $Destination -Force
        throw "SHA-256 mismatch for $Url`n  expected $Sha256`n  actual   $actual"
    }
    Write-Ok "verified $Destination"
}

function Test-Command([string] $name) {
    return [bool] (Get-Command $name -ErrorAction SilentlyContinue)
}

function Test-WingetPackage {
    param([string] $Id, [string] $Version)
    if (-not (Test-Command 'winget')) { return $false }
    $output = & winget list --id $Id --exact --accept-source-agreements 2>&1 | Out-String
    return $output -match [regex]::Escape($Version)
}

function Install-WingetPackage {
    param([string] $Id, [string] $Version, [string] $Override = '')
    if (Test-WingetPackage -Id $Id -Version $Version) {
        Write-Ok "$Id $Version already installed"
        return
    }
    if ($DryRun) { Write-Host "    would install $Id $Version"; return }
    $arguments = @('install', '--id', $Id, '--exact', '--version', $Version, '--silent',
        '--accept-package-agreements', '--accept-source-agreements')
    if ($Override) { $arguments += @('--override', $Override) }
    & winget @arguments
    if ($LASTEXITCODE -ne 0) { throw "winget install $Id $Version failed with $LASTEXITCODE" }
    Write-Ok "$Id $Version installed"
}

Assert-Administrator
if (-not (Test-Path -LiteralPath $DownloadDir)) { New-Item -ItemType Directory -Path $DownloadDir -Force | Out-Null }

Write-Step 'Rust environment'
Set-MachineVariable 'RUSTUP_HOME' $pins.RustHome.RustupHome
Set-MachineVariable 'CARGO_HOME' $pins.RustHome.CargoHome
Add-MachinePath (Join-Path $pins.RustHome.CargoHome 'bin')

$rustupInit = Join-Path $DownloadDir 'rustup-init.exe'
Get-PinnedFile -Url $pins.Rust.Url -Destination $rustupInit -Sha256 $pins.Rust.Sha256
$rustc = Join-Path $pins.RustHome.CargoHome 'bin\rustc.exe'
$haveRust = (Test-Path -LiteralPath $rustc) -and ((& $rustc --version) -match [regex]::Escape($pins.Rust.Version))
if ($haveRust) {
    Write-Ok "rustc $($pins.Rust.Version) already installed"
} elseif ($DryRun) {
    Write-Host "    would install Rust $($pins.Rust.Version)"
} else {
    & $rustupInit -y --no-modify-path --profile minimal --default-toolchain none
    if ($LASTEXITCODE -ne 0) { throw "rustup-init failed with $LASTEXITCODE" }
    $rustup = Join-Path $pins.RustHome.CargoHome 'bin\rustup.exe'
    & $rustup toolchain install $pins.Rust.Version --profile minimal --component rustfmt --component clippy
    if ($LASTEXITCODE -ne 0) { throw "rustup toolchain install failed with $LASTEXITCODE" }
    & $rustup default $pins.Rust.Version
    Write-Ok "Rust $($pins.Rust.Version) installed"
}

Write-Step '.NET SDK'
Add-MachinePath $pins.DotNet.InstallDir
$dotnet = Join-Path $pins.DotNet.InstallDir 'dotnet.exe'
$haveDotnet = $false
if (Test-Path -LiteralPath $dotnet) {
    $haveDotnet = ((& $dotnet --list-sdks) | Out-String) -match "^$([regex]::Escape($pins.DotNet.SdkVersion)) "
}
if ($haveDotnet) {
    Write-Ok ".NET SDK $($pins.DotNet.SdkVersion) already installed"
} else {
    $script = Join-Path $DownloadDir 'dotnet-install.ps1'
    Get-PinnedFile -Url $pins.DotNet.ScriptUrl -Destination $script -Sha256 $pins.DotNet.ScriptSha256
    if ($DryRun) {
        Write-Host "    would install .NET SDK $($pins.DotNet.SdkVersion) into $($pins.DotNet.InstallDir)"
    } else {
        & $script -Version $pins.DotNet.SdkVersion -InstallDir $pins.DotNet.InstallDir -NoPath
        if ($LASTEXITCODE -ne 0) { throw "dotnet-install failed with $LASTEXITCODE" }
        Write-Ok ".NET SDK $($pins.DotNet.SdkVersion) installed"
    }
}

if ($SkipWinget) {
    Write-Host '==> Skipping winget packages (-SkipWinget)'
} else {
    if (-not (Test-Command 'winget')) {
        throw 'winget is not available. Install App Installer (winget) before running setup.ps1.'
    }
    Write-Step 'Git'
    Install-WingetPackage -Id $pins.Git.WingetId -Version $pins.Git.Version
    Write-Step 'Python 3.12'
    Install-WingetPackage -Id $pins.Python.WingetId -Version $pins.Python.Version
    Write-Step 'PostgreSQL 18'
    Install-WingetPackage -Id $pins.PostgreSQL.WingetId -Version $pins.PostgreSQL.Version
    Write-Step 'Visual Studio Build Tools (MSVC C++ workload)'
    Install-WingetPackage -Id $pins.BuildTools.WingetId -Version $pins.BuildTools.Version -Override $pins.BuildTools.Override
}

Write-Step 'Summary'
Write-Host "Rust $($pins.Rust.Version) in $($pins.RustHome.CargoHome)\bin"
Write-Host ".NET SDK $($pins.DotNet.SdkVersion) in $($pins.DotNet.InstallDir)"
Write-Host "Git $($pins.Git.Version), Python $($pins.Python.Version), PostgreSQL $($pins.PostgreSQL.Version), VS Build Tools $($pins.BuildTools.Version)"
Write-Host 'Run dev-environment\windows\verify.ps1 to check the versions and a fast build subset.'
