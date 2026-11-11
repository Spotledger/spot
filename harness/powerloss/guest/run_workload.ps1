<#
.SYNOPSIS
    Workload agent for the power-loss harness, run inside the Windows guest (WP-PRD-05.T1).

.DESCRIPTION
    The orchestrator invokes this on the target over SSH (or the Hyper-V guest agent). It
    runs the workload agent (guest/run_workload.py) with the seed, the history path and the
    cut point, and writes a durable client-side history. It never shells out to build a
    command line: the agent is invoked with an argument array, so a path with spaces cannot
    be re-parsed.

.EXAMPLE
    .\run_workload.ps1 -Seed 7 -History C:\pl\history.jsonl -CutAfter 500 -Python python
#>
[CmdletBinding()]
param(
    [Parameter(Mandatory = $true)][int]$Seed,
    [Parameter(Mandatory = $true)][string]$History,
    [int]$CutAfter = 0,
    [int]$Workers = 8,
    [int]$Operations = 2000,
    [string]$Python = "python",
    [string]$Agent = ""
)

$ErrorActionPreference = "Stop"
$here = Split-Path -Parent $MyInvocation.MyCommand.Path
if ([string]::IsNullOrEmpty($Agent)) { $Agent = Join-Path $here "run_workload.py" }

$arguments = @(
    $Agent,
    "--seed", "$Seed",
    "--history", $History,
    "--cut-after", "$CutAfter",
    "--workers", "$Workers",
    "--operations", "$Operations",
    "--store", "model"
)

& $Python @arguments
exit $LASTEXITCODE
