#Requires -Version 7.0
<#
.SYNOPSIS
    Run the WP-ADM-05 Windows module conformance suite on a disposable Windows VM.

.DESCRIPTION
    Builds and runs the standalone `spot-agent-windows-real` cargo test crate in
    `tests-real/` against the real Windows modules, collects JUnit XML and prints the
    matrix from `matrix.toml` (via `orchestrator.py`, the single parser of that file).

    The suite creates and deletes a throw-away service, a local user and a firewall rule
    (and, for the vss test, a shadow copy). Every object it touches is named with the
    unique prefix it generates per run, and every test removes its own objects on every
    path, including a panic; this script additionally sweeps any object still carrying
    the prefix in a `finally` block.

    Safety guards: the script refuses to run unless SPOT_HARNESS_DISPOSABLE=1 is set and
    the machine looks like a virtual machine (Win32_ComputerSystem.Model). It must only
    ever run on a disposable Windows VM runner.

.PARAMETER Target
    The matrix target this machine is (for example windows-server-2022).

.PARAMETER Test
    Run only this one test (its cargo integration-test target name, for example services).

.PARAMETER Artifacts
    Where the JUnit XML is written. Defaults to harness/agent-windows/artifacts.

.PARAMETER ListMatrix
    Print the matrix and exit, without any guard or machine change.

.PARAMETER Json
    Emit one JSON outcome object on stdout (used by orchestrator.py's PwshRunner).

.PARAMETER SkipBuild
    Skip the `cargo test --no-run` build step (the binary is expected to be current).

.EXAMPLE
    ./run.ps1 -ListMatrix
    $env:SPOT_HARNESS_DISPOSABLE = '1'; ./run.ps1 -Target windows-server-2022
    ./run.ps1 -Target windows-11 -Test services -Json
#>
[CmdletBinding()]
param(
    [string] $Target,
    [string] $Test,
    [string] $Artifacts = (Join-Path $PSScriptRoot 'artifacts'),
    [switch] $ListMatrix,
    [switch] $Json,
    [switch] $SkipBuild
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$HarnessDir = $PSScriptRoot
$TestsReal = Join-Path $HarnessDir 'tests-real'
$Orchestrator = Join-Path $HarnessDir 'orchestrator.py'
$CrateName = 'spot-agent-windows-real'
$PrefixRoot = 'spot-h-'

#: When -Json is set the script prints nothing but the final JSON object on stdout.
$script:Quiet = [bool] $Json

function Write-Status {
    param([string] $Message)
    if (-not $script:Quiet) { Write-Host $Message }
}

# --- reading the matrix through orchestrator.py (the single parser of matrix.toml) ------

function Get-PythonCommand {
    foreach ($candidate in @('python', 'python3', 'py')) {
        $found = Get-Command $candidate -ErrorAction SilentlyContinue
        if ($null -ne $found) {
            $prefix = @()
            if ($candidate -eq 'py') { $prefix = @('-3') }
            return [pscustomobject]@{ Exe = $found.Source; Prefix = $prefix }
        }
    }
    throw 'Python 3.11 or newer is required on the Windows runner; none was found on PATH.'
}

function Invoke-Python {
    param([Parameter(ValueFromRemainingArguments = $true)][string[]] $Arguments)
    $python = Get-PythonCommand
    $all = @($python.Prefix) + $Arguments
    $output = & $python.Exe @all 2>&1
    if ($LASTEXITCODE -ne 0) {
        throw "python $($Arguments -join ' ') failed with exit $LASTEXITCODE`: $output"
    }
    return ($output | Out-String)
}

function Get-Matrix {
    $json = Invoke-Python $Orchestrator 'matrix' '--json'
    return ($json | ConvertFrom-Json -AsHashtable)
}

# --- safety guards ----------------------------------------------------------------------

function Assert-Disposable {
    if ($env:SPOT_HARNESS_DISPOSABLE -ne '1') {
        throw 'Refusing to run: set SPOT_HARNESS_DISPOSABLE=1 to confirm this machine is disposable. ' +
              'The suite creates and deletes a service, a local user and a firewall rule.'
    }
    $system = Get-CimInstance -ClassName Win32_ComputerSystem
    $model = "$($system.Manufacturer) $($system.Model)".Trim()
    $vmPattern = 'Virtual|VMware|VirtualBox|KVM|QEMU|Xen|Parallels|Hyper-V|Bochs|Amazon EC2|Google Compute Engine|OpenStack|Droplet|Cloud PC'
    $isVirtual = ($model -match $vmPattern) -or [bool] $system.HypervisorPresent
    if (-not $isVirtual) {
        throw "Refusing to run: '$model' (Win32_ComputerSystem.Model) is not a virtual machine. " +
              'The suite only runs on the disposable Windows VM runner.'
    }
    Write-Status "disposable VM confirmed: $model"
}

function Test-Capability {
    param([string] $Name)
    switch ($Name) {
        'admin' {
            $identity = [System.Security.Principal.WindowsIdentity]::GetCurrent()
            $principal = [System.Security.Principal.WindowsPrincipal]::new($identity)
            return $principal.IsInRole([System.Security.Principal.WindowsBuiltInRole]::Administrator)
        }
        'vss' {
            return $null -ne (Get-Service -Name 'VSS' -ErrorAction SilentlyContinue)
        }
        'update_service' {
            return $null -ne (Get-Service -Name 'wuauserv' -ErrorAction SilentlyContinue)
        }
        'firewall' {
            return $null -ne (Get-Service -Name 'mpssvc' -ErrorAction SilentlyContinue)
        }
        'netapi' {
            try {
                $null = Get-LocalUser -ErrorAction Stop
                return $true
            } catch {
                return $false
            }
        }
        default {
            throw "unknown capability '$Name' in matrix.toml"
        }
    }
}

# --- the throw-away prefix --------------------------------------------------------------

function New-Prefix {
    $suffix = -join (1..8 | ForEach-Object { '{0:x}' -f (Get-Random -Minimum 0 -Maximum 16) })
    # Short enough that `<prefix>-user` stays inside the 20-character local-account limit.
    return "$PrefixRoot$suffix"
}

# --- running the cargo tests ------------------------------------------------------------

function Invoke-CargoBuild {
    Write-Status "building $CrateName ..."
    Push-Location $TestsReal
    try {
        $output = & cargo test -p $CrateName --no-run 2>&1
        if ($LASTEXITCODE -ne 0) {
            throw "cargo test --no-run failed with exit $LASTEXITCODE`: $output"
        }
    } finally {
        Pop-Location
    }
}

function Invoke-CargoTest {
    param([string] $TestName, [string] $Prefix, [string] $TargetName)
    $env:SPOT_HARNESS_PREFIX = $Prefix
    $env:SPOT_HARNESS_TARGET = $TargetName
    $lines = @()
    $exit = 1
    Push-Location $TestsReal
    try {
        # --test-threads=1: the tests change machine state; running them serially keeps
        # one test's throw-away objects out of another test's enumeration.
        $lines = @(& cargo test -p $CrateName --test $TestName -- --test-threads=1 2>&1)
        $exit = $LASTEXITCODE
    } finally {
        Pop-Location
        Remove-Item Env:\SPOT_HARNESS_PREFIX -ErrorAction SilentlyContinue
        Remove-Item Env:\SPOT_HARNESS_TARGET -ErrorAction SilentlyContinue
    }
    return [pscustomobject]@{ Lines = $lines; ExitCode = $exit }
}

# --- JUnit ------------------------------------------------------------------------------

function ConvertTo-JUnitCases {
    param([string[]] $Lines)
    $cases = [System.Collections.Generic.List[object]]::new()
    foreach ($line in $Lines) {
        if ($line -match '^test\s+(?<name>\S.*?)\s+\.\.\.\s+(?<result>ok|FAILED|ignored)') {
            $cases.Add([pscustomobject]@{ Name = $Matches['name']; Result = $Matches['result'] })
        }
    }
    return $cases
}

function New-JUnitXml {
    param([string] $SuiteName, [object[]] $Cases)
    $settings = [System.Xml.XmlWriterSettings]::new()
    $settings.Indent = $true
    $settings.OmitXmlDeclaration = $false
    $builder = [System.Text.StringBuilder]::new()
    $writer = [System.Xml.XmlWriter]::Create($builder, $settings)
    try {
        $writer.WriteStartDocument()
        $writer.WriteStartElement('testsuites')
        $writer.WriteStartElement('testsuite')
        $writer.WriteAttributeString('name', $SuiteName)
        $writer.WriteAttributeString('tests', $Cases.Count.ToString())
        $failures = @($Cases | Where-Object { $_.Result -eq 'FAILED' }).Count
        $skipped = @($Cases | Where-Object { $_.Result -eq 'ignored' }).Count
        $writer.WriteAttributeString('failures', $failures.ToString())
        $writer.WriteAttributeString('skipped', $skipped.ToString())
        foreach ($case in $Cases) {
            $writer.WriteStartElement('testcase')
            $writer.WriteAttributeString('classname', $SuiteName)
            $writer.WriteAttributeString('name', $case.Name)
            if ($case.Result -eq 'FAILED') {
                $writer.WriteStartElement('failure')
                $writer.WriteAttributeString('message', 'test failed')
                $writer.WriteEndElement()
            } elseif ($case.Result -eq 'ignored') {
                $writer.WriteStartElement('skipped')
                $writer.WriteEndElement()
            }
            $writer.WriteEndElement()
        }
        $writer.WriteEndElement()
        $writer.WriteEndElement()
        $writer.WriteEndDocument()
    } finally {
        $writer.Dispose()
    }
    return $builder.ToString()
}

function Write-JUnit {
    param([string] $TargetName, [string] $TestName, [string] $Xml)
    $directory = Join-Path $Artifacts (Join-Path 'junit' $TargetName)
    New-Item -ItemType Directory -Force -Path $directory | Out-Null
    $path = Join-Path $directory "$TestName.xml"
    Set-Content -Path $path -Value $Xml -Encoding utf8
    return $path
}

# --- one test on one target -------------------------------------------------------------

function Invoke-Test {
    param([hashtable] $Matrix, [string] $TargetName, [string] $TestName, [string] $Prefix)

    $test = @($Matrix['tests'] | Where-Object { $_['name'] -eq $TestName })[0]
    if ($null -eq $test) { throw "unknown test '$TestName'" }
    $target = @($Matrix['targets'] | Where-Object { $_['name'] -eq $TargetName })[0]
    if ($null -eq $target) { throw "unknown target '$TargetName'" }

    $missing = [System.Collections.Generic.List[string]]::new()
    foreach ($capability in $test['capabilities']) {
        if ($target['capabilities'] -notcontains $capability) {
            $missing.Add($capability)
        } elseif (-not (Test-Capability $capability)) {
            $missing.Add($capability)
        }
    }
    if ($missing.Count -gt 0) {
        $message = "missing capabilities: $($missing -join ', ')"
        Write-Status "[skip] $TargetName/$TestName $message"
        return [pscustomobject]@{
            Target = $TargetName; Test = $TestName; Status = 'skip'
            Passed = 0; Failed = 0; Skipped = 0; Message = $message; JunitPath = ''
        }
    }

    Write-Status "[run ] $TargetName/$TestName"
    $run = Invoke-CargoTest -TestName $TestName -Prefix $Prefix -TargetName $TargetName
    $cases = ConvertTo-JUnitCases -Lines $run.Lines
    if ($cases.Count -eq 0) {
        # A build or harness failure with no per-test lines must still be one failing case,
        # never a silent pass.
        $cases.Add([pscustomobject]@{ Name = "$TestName (cargo)"; Result = 'FAILED' })
    }
    $junit = New-JUnitXml -SuiteName "$TargetName/$TestName" -Cases $cases
    $path = Write-JUnit -TargetName $TargetName -TestName $TestName -Xml $junit
    $failed = @($cases | Where-Object { $_.Result -eq 'FAILED' }).Count
    $passed = @($cases | Where-Object { $_.Result -eq 'ok' }).Count
    $skipped = @($cases | Where-Object { $_.Result -eq 'ignored' }).Count
    $status = if ($run.ExitCode -eq 0 -and $failed -eq 0) { 'pass' } else { 'fail' }
    $message = if ($status -eq 'pass') { "$passed passed" } else { "$failed failed (exit $($run.ExitCode))" }
    Write-Status "[$status] $TargetName/$TestName $message"
    return [pscustomobject]@{
        Target = $TargetName; Test = $TestName; Status = $status
        Passed = $passed; Failed = $failed; Skipped = $skipped
        Message = $message; JunitPath = $path
    }
}

# --- belt-and-braces cleanup ------------------------------------------------------------

function Remove-Leftovers {
    param([string] $Prefix)
    if ([string]::IsNullOrWhiteSpace($Prefix) -or $Prefix.Length -lt 12) {
        throw 'refusing to sweep with a missing or too-short prefix'
    }
    Get-Service -ErrorAction SilentlyContinue |
        Where-Object { $_.Name.StartsWith($Prefix) } |
        ForEach-Object { & sc.exe delete $_.Name | Out-Null }
    Get-LocalUser -ErrorAction SilentlyContinue |
        Where-Object { $_.Name.StartsWith($Prefix) } |
        ForEach-Object { Remove-LocalUser -Name $_.Name -ErrorAction SilentlyContinue }
    Get-NetFirewallRule -ErrorAction SilentlyContinue |
        Where-Object { $_.DisplayName.StartsWith($Prefix) } |
        ForEach-Object { Remove-NetFirewallRule -Name $_.Name -ErrorAction SilentlyContinue }
}

# --- main -------------------------------------------------------------------------------

if ($ListMatrix) {
    Write-Host (Invoke-Python $Orchestrator 'matrix')
    exit 0
}

$matrix = Get-Matrix

if (-not $Target) {
    $known = ($matrix['targets'] | ForEach-Object { $_['name'] }) -join ', '
    throw "-Target is required. Known targets: $known"
}
$targetEntry = @($matrix['targets'] | Where-Object { $_['name'] -eq $Target })[0]
if ($null -eq $targetEntry) {
    $known = ($matrix['targets'] | ForEach-Object { $_['name'] }) -join ', '
    throw "unknown target '$Target'. Known targets: $known"
}

Assert-Disposable

$prefix = New-Prefix
$testNames = if ($Test) { @($Test) } else { @($matrix['tests'] | ForEach-Object { $_['name'] }) }

if (-not $SkipBuild) { Invoke-CargoBuild }

$outcomes = [System.Collections.Generic.List[object]]::new()
try {
    foreach ($testName in $testNames) {
        $outcomes.Add((Invoke-Test -Matrix $matrix -TargetName $Target -TestName $testName -Prefix $prefix))
    }
} finally {
    Remove-Leftovers -Prefix $prefix
}

if ($Json) {
    $outcome = $outcomes[0]
    # snake_case keys: this object is parsed by orchestrator.py's PwshRunner.
    [ordered]@{
        target     = $outcome.Target
        test       = $outcome.Test
        status     = $outcome.Status
        passed     = $outcome.Passed
        failed     = $outcome.Failed
        skipped    = $outcome.Skipped
        message    = $outcome.Message
        junit_path = $outcome.JunitPath
    } | ConvertTo-Json -Compress
}

$failures = @($outcomes | Where-Object { $_.Status -eq 'fail' })
if ($failures.Count -gt 0) { exit 1 }
exit 0
