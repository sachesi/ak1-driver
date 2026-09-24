#Requires -Version 7
<#
.SYNOPSIS
    Builds release binaries and collects everything install.ps1 needs into
    target\bundle: the signed driver package, the signing certificate, the ASIO
    driver, the control panel and the test tools.
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
Copy-Item (Join-Path $PSScriptRoot 'install.ps1') $bundle
Export-Certificate -Cert "Cert:\CurrentUser\My\$CertThumbprint" -FilePath (Join-Path $bundle 'ak1-driver-test.cer') | Out-Null
Write-Output "bundle: $bundle"
