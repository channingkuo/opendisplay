# Builds the Windows receiver and writes it as "OpenDisplay Receiver.exe"
# (the Mac receiver's product name; Cargo cannot name a binary with a space)
# into windows\dist.
#
#   .\build.ps1            # release build
#   .\build.ps1 -Debug     # debug build
param([switch]$Debug)

$ErrorActionPreference = 'Stop'
Set-Location $PSScriptRoot

$profile = if ($Debug) { 'debug' } else { 'release' }
if ($Debug) { cargo build } else { cargo build --release }
if ($LASTEXITCODE -ne 0) { exit $LASTEXITCODE }

New-Item -ItemType Directory -Force -Path dist | Out-Null
$out = Join-Path dist 'OpenDisplay Receiver.exe'
Copy-Item "target\$profile\od-receiver-win.exe" $out -Force
Write-Host "Built $out"
