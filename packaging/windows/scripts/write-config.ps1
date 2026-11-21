# Writes <DataDir>\config\spot.toml from the silent-install properties (WP-PRD-03.T4).
# Runs as SYSTEM from the installation folder (admin-only). Every input is validated; nothing is
# executed from a value, and no secret passes through here.
param(
    [Parameter(Mandatory = $true)][string]$DataDir,
    [Parameter(Mandatory = $true)][string]$Port,
    [string]$AdminEmail = '',
    [string]$LanDiscovery = '0'
)
$ErrorActionPreference = 'Stop'

if (-not [System.IO.Path]::IsPathRooted($DataDir)) { throw 'DATADIR must be an absolute path.' }
if ($Port -notmatch '^[0-9]{1,5}$' -or [int]$Port -lt 1 -or [int]$Port -gt 65535) { throw 'HTTP_PORT must be 1-65535.' }
if ($AdminEmail -ne '' -and $AdminEmail -notmatch '^[^\s"\\@]+@[^\s"\\@]+$') { throw 'ADMIN_EMAIL is not an e-mail address.' }
if ($LanDiscovery -ne '0' -and $LanDiscovery -ne '1') { throw 'LAN_DISCOVERY must be 0 or 1.' }

$config = Join-Path $DataDir 'config'
New-Item -ItemType Directory -Force -Path $config | Out-Null
$lan = if ($LanDiscovery -eq '1') { 'true' } else { 'false' }
$lines = @(
    '# Written by the Spotledger installer. Edit with care; an upgrade keeps this file.',
    "data_dir = '$($DataDir -replace "'", '')'",
    "http_port = $Port",
    "lan_discovery = $lan"
)
if ($AdminEmail -ne '') { $lines += "admin_email = '$AdminEmail'" }
$target = Join-Path $config 'spot.toml'
if (-not (Test-Path $target)) {
    Set-Content -Path $target -Value $lines -Encoding UTF8
}
