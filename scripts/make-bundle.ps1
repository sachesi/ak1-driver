#Requires -Version 7
<#
.SYNOPSIS
    Builds release binaries and collects them in target\bundle: the signed
    driver package, the signing certificate, the ASIO driver, the control panel,
    the test tools and ak1-setup.exe, which carries the files it installs.
.PARAMETER CertThumbprint
    SHA-1 thumbprint of the code signing certificate in Cert:\CurrentUser\My.
#>
param([Parameter(Mandatory)] [string] $CertThumbprint)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
$bundle = Join-Path $root 'target\bundle'

& (Join-Path $PSScriptRoot 'package-driver.ps1') -Release -CertThumbprint $CertThumbprint
& cargo build --release -p ak1-asio -p ak1-panel -p ak1-probe -p ak1-asio-host
if ($LASTEXITCODE) { throw "cargo build failed with exit code $LASTEXITCODE" }

Remove-Item $bundle -Recurse -Force -ErrorAction Ignore
New-Item -ItemType Directory $bundle | Out-Null
Copy-Item (Join-Path $root 'target\release\package\ak1acx.*') $bundle -Exclude *.pdb
foreach ($file in 'ak1_asio.dll', 'ak1-panel.exe', 'ak1-probe.exe', 'ak1-asio-host.exe') {
    Copy-Item (Join-Path $root "target\release\$file") $bundle
}
Export-Certificate -Cert "Cert:\CurrentUser\My\$CertThumbprint" -FilePath (Join-Path $bundle 'ak1-driver-test.cer') | Out-Null

$env:AK1_SETUP_PAYLOAD = $bundle
try {
    & cargo build --release -p ak1-setup
    if ($LASTEXITCODE) { throw "cargo build failed with exit code $LASTEXITCODE" }
} finally {
    Remove-Item Env:\AK1_SETUP_PAYLOAD
}
Copy-Item (Join-Path $root 'target\release\ak1-setup.exe') $bundle
Write-Output "bundle: $bundle"
