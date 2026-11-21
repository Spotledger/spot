#Requires -Version 5.1
<#
.SYNOPSIS
    Verify a Windows Spotledger dev environment (WP-PRD-10.T1).

.DESCRIPTION
    Checks the pinned toolchain versions from versions.psd1 and then builds and
    tests a fast subset: cargo fmt over the workspace and the gate-matrix unit
    tests. It is a smoke test of the runner image, not the full CI run (that is
    the nightly workflow). Runs as the ordinary runner user.

.EXAMPLE
    powershell -ExecutionPolicy Bypass -File dev-environment\windows\verify.ps1
#>
[CmdletBinding()]
param(
    [string] $RepoRoot = (Resolve-Path (Join-Path $PSScriptRoot '..\..')).Path
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

$pins = Import-PowerShellDataFile -Path (Join-Path $PSScriptRoot 'versions.psd1')
$failures = New-Object System.Collections.Generic.List[string]

function Write-Ok([string] $message) { Write-Host "verify: OK $message" -ForegroundColor Green }
function Fail([string] $message) {
    Write-Host "verify: FAIL $message" -ForegroundColor Red
    $failures.Add($message)
}

function Require-Command([string] $name) {
    if (-not (Get-Command $name -ErrorAction SilentlyContinue)) {
        Fail "$name is not on PATH"
        return $false
    }
    return $true
}

function Expect-Match([string] $what, [string] $actual, [string] $pattern) {
    if ($actual -match $pattern) { Write-Ok "$what $actual" } else { Fail "$what is '$actual', expected /$pattern/" }
}

if (Require-Command 'rustc') {
    Expect-Match 'rustc' (& rustc --version | ForEach-Object { ($_ -split ' ')[1] }) ([regex]::Escape($pins.Rust.Version))
}
if (Require-Command 'cargo') {
    Expect-Match 'cargo' (& cargo --version | ForEach-Object { ($_ -split ' ')[1] }) ([regex]::Escape($pins.Rust.Version))
    $components = (& rustup component list --installed) -join "`n"
    foreach ($component in @('rustfmt', 'clippy')) {
        if ($components -match $component) { Write-Ok "$component installed" } else { Fail "$component is not installed" }
    }
}

if (Require-Command 'dotnet') {
    $sdks = (& dotnet --list-sdks) -join "`n"
    Expect-Match '.NET SDK' $sdks "^$([regex]::Escape($pins.DotNet.SdkVersion)) "
}

if (Require-Command 'python') {
    Expect-Match 'python' (& python --version 2>&1 | ForEach-Object { ($_ -split ' ')[1] }) '^3\.12\.'
}

if (Require-Command 'psql') {
    Expect-Match 'psql' (& psql --version | ForEach-Object { ($_ -split ' ')[2] }) '^18\.'
}

if (Require-Command 'git') {
    Expect-Match 'git' (& git --version | ForEach-Object { ($_ -split ' ')[2] }) '^2\.'
}

if (@($failures).Count -eq 0) {
    Write-Host 'verify: building a fast subset'
    Push-Location $RepoRoot
    try {
        & cargo fmt --all -- --check
        if ($LASTEXITCODE -ne 0) { throw 'cargo fmt --check failed' }
        & python -m unittest discover -s tools/ci/matrix/tests
        if ($LASTEXITCODE -ne 0) { throw 'the gate-matrix unit tests failed' }
    } finally {
        Pop-Location
    }
    Write-Ok 'Windows dev environment is ready'
} else {
    Write-Host "verify: $($failures.Count) check(s) failed" -ForegroundColor Red
    exit 1
}
