#requires -Version 5.1
<#
.SYNOPSIS
Opens the pool dashboard (`cargo xtask pool`) on the KVM host over SSH.
.DESCRIPTION
Runs on the Windows workstation. Over `ssh -t` it builds the xtask binary in
a checkout of this repository on the KVM host (as the SSH user; Cargo never
runs as root), then runs that prebuilt binary through sudo so the dashboard
can read the controller's configuration and journal. The dashboard is
read-only. Ctrl+C stops it and leaves the last view on screen to copy.

The host and checkout are site values. Pass them, or set them once in the
environment or in site/pool.env (untracked; copy
deploy/examples/pool.env.example):
  VIRTIO_MEM_POOL_HOST      SSH destination of the KVM host
  VIRTIO_MEM_POOL_CHECKOUT  Path of this repository's checkout on that host
  VIRTIO_MEM_POOL_CONFIG    Host list; default /etc/virtio-mem-host/host_list.toml
.EXAMPLE
.\Watch-Pool.ps1
.EXAMPLE
.\Watch-Pool.ps1 -NewWindow
.EXAMPLE
.\Watch-Pool.ps1 -HostName kvm-host -Checkout '~/src/virtio-mem-pool' -Update
#>
[CmdletBinding()]
param(
    [string] $HostName,
    [string] $Checkout,
    [string] $Config,
    [ValidateRange(0.1, 3600)]
    [double] $IntervalSeconds = 1,
    [ValidateRange(1, 30)]
    [int] $Events = 8,
    # Fast-forward the host's checkout from its remote before building.
    [switch] $Update,
    # Open the dashboard in a separate, suitably sized console window.
    [switch] $NewWindow
)

Set-StrictMode -Version Latest
$ErrorActionPreference = 'Stop'

# Explicit parameters win, then the environment, then site/pool.env.
$site = @{}
$siteFile = Join-Path $PSScriptRoot '..\..\site\pool.env'
if (Test-Path -LiteralPath $siteFile) {
    foreach ($line in Get-Content -LiteralPath $siteFile) {
        if ($line -match '^\s*([A-Z_]+)\s*=\s*(.*?)\s*$') { $site[$Matches[1]] = $Matches[2] }
    }
}
function Resolve-Setting([string] $Value, [string] $Name, [string] $Default) {
    if ($Value) { return $Value }
    $fromEnvironment = [Environment]::GetEnvironmentVariable($Name)
    if ($fromEnvironment) { return $fromEnvironment }
    if ($site.ContainsKey($Name) -and $site[$Name]) { return $site[$Name] }
    return $Default
}
$HostName = Resolve-Setting $HostName 'VIRTIO_MEM_POOL_HOST' ''
$Checkout = Resolve-Setting $Checkout 'VIRTIO_MEM_POOL_CHECKOUT' ''
$Config = Resolve-Setting $Config 'VIRTIO_MEM_POOL_CONFIG' '/etc/virtio-mem-host/host_list.toml'
if (-not $HostName -or -not $Checkout) {
    throw 'Name the KVM host and its checkout: -HostName and -Checkout, ' +
        'VIRTIO_MEM_POOL_HOST and VIRTIO_MEM_POOL_CHECKOUT, or site/pool.env ' +
        '(see deploy/examples/pool.env.example).'
}
foreach ($value in @($HostName, $Checkout, $Config)) {
    if ($value -match '[\s''"`$\\;&|<>]') {
        throw "Refusing a host, checkout or config with spaces or shell characters: $value"
    }
}

if ($NewWindow) {
    $arguments = @(
        '-NoExit', '-ExecutionPolicy', 'Bypass', '-Command',
        ("mode con cols=140 lines=52; & '{0}' -HostName '{1}' -Checkout '{2}' -Config '{3}' " +
            '-IntervalSeconds {4} -Events {5}{6}') -f $PSCommandPath, $HostName, $Checkout,
            $Config, $IntervalSeconds.ToString([Globalization.CultureInfo]::InvariantCulture),
            $Events, $(if ($Update) { ' -Update' } else { '' })
    )
    Start-Process powershell -ArgumentList $arguments
    return
}

# A leading ~/ means the SSH user's home; everything else is used as given.
$remoteCheckout = if ($Checkout -like '~/*') { '$HOME/' + $Checkout.Substring(2) } else { $Checkout }
$interval = $IntervalSeconds.ToString([Globalization.CultureInfo]::InvariantCulture)
$script = @"
set -eu
cd -- "$remoteCheckout"
if [ -f "`$HOME/.cargo/env" ]; then . "`$HOME/.cargo/env"; fi
$(if ($Update) { "echo '==> updating the checkout'; git pull --ff-only" })
echo '==> building virtio-mem-xtask'
cargo build --quiet --release -p virtio-mem-xtask
exec sudo target/release/virtio-mem-xtask pool --config '$Config' --interval-seconds $interval --events $Events
"@
# Base64 keeps the script clear of Windows PowerShell's native-argument
# quoting; process substitution keeps the terminal as bash's stdin, so sudo
# can prompt and Ctrl+C reaches the dashboard.
$encoded = [Convert]::ToBase64String([Text.Encoding]::UTF8.GetBytes($script.Replace("`r`n", "`n")))
& ssh -t $HostName "bash <(echo $encoded | base64 -d)"
if ($LASTEXITCODE -ne 0 -and $LASTEXITCODE -ne 130) {
    Write-Warning "ssh ended with exit code $LASTEXITCODE"
}
