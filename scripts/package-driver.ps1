#Requires -Version 7
<#
.SYNOPSIS
    Builds ak1acx.sys and produces a signed driver package under target\<profile>\package.
.PARAMETER Release
    Build with the release profile.
.PARAMETER CertThumbprint
    SHA-1 thumbprint of a code signing certificate in Cert:\CurrentUser\My.
#>
param(
    [switch] $Release,
    [Parameter(Mandatory)] [string] $CertThumbprint
)

$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
$profileName = $Release ? 'release' : 'debug'
$kits = (Get-ItemProperty 'HKLM:\SOFTWARE\Microsoft\Windows Kits\Installed Roots').KitsRoot10
$kitVersion = (Get-ChildItem "$kits\bin" -Directory | Where-Object Name -Match '^10\.' |
    Sort-Object { [version]$_.Name } | Select-Object -Last 1).Name
$bin = Join-Path $kits "bin\$kitVersion"

if (-not $env:LIBCLANG_PATH) { $env:LIBCLANG_PATH = 'C:\Program Files\LLVM\bin' }
$cargoArgs = @('build', '-p', 'ak1-acx') + ($Release ? @('--release') : @())
& cargo @cargoArgs
if ($LASTEXITCODE) { throw "cargo build failed with exit code $LASTEXITCODE" }

$package = Join-Path $root "target\$profileName\package"
Remove-Item $package -Recurse -Force -ErrorAction Ignore
New-Item -ItemType Directory $package | Out-Null
Copy-Item (Join-Path $root "target\$profileName\ak1_acx.dll") (Join-Path $package 'ak1acx.sys')
Copy-Item (Join-Path $root "target\$profileName\ak1_acx.pdb") (Join-Path $package 'ak1acx.pdb')
Copy-Item (Join-Path $root 'driver\ak1-acx\ak1acx.inf') $package
$inf = Join-Path $package 'ak1acx.inf'

# The commit count keeps versions increasing without depending on the build machine's clock.
$version = "0.1.0.$(git -C $root rev-list --count HEAD)"
& "$bin\x64\stampinf.exe" -f $inf -d * -v $version -a amd64 -k 1.33 -x
if ($LASTEXITCODE) { throw "stampinf failed with exit code $LASTEXITCODE" }
& "$bin\x86\inf2cat.exe" /driver:$package /os:10_x64 /uselocaltime
if ($LASTEXITCODE) { throw "inf2cat failed with exit code $LASTEXITCODE" }
foreach ($file in 'ak1acx.sys', 'ak1acx.cat') {
    & "$bin\x64\signtool.exe" sign /sha1 $CertThumbprint /fd sha256 (Join-Path $package $file)
    if ($LASTEXITCODE) { throw "signtool failed on $file with exit code $LASTEXITCODE" }
}
Write-Output "package: $package"
