#Requires -Version 7
#Requires -RunAsAdministrator
<#
.SYNOPSIS
    Installs or removes the Audio Kontrol 1 driver, ASIO driver and control
    panel from a bundle made by make-bundle.ps1. Run it from the bundle directory.
.PARAMETER Uninstall
    Remove the ASIO registration, the driver package, the installed files and
    the Start menu shortcut.
#>
param([switch] $Uninstall)

$ErrorActionPreference = 'Stop'
$here = $PSScriptRoot
$asioDir = Join-Path $env:ProgramFiles 'Audio Kontrol 1'
$asioDll = Join-Path $asioDir 'ak1_asio.dll'
$panel = Join-Path $asioDir 'ak1-panel.exe'
$shortcut = Join-Path $env:ProgramData 'Microsoft\Windows\Start Menu\Programs\Audio Kontrol 1 Control Panel.lnk'

# regsvr32 is a GUI program; the call operator would not wait for it.
function Invoke-Regsvr32([string[]] $Arguments) {
    (Start-Process regsvr32.exe -ArgumentList $Arguments -Wait -PassThru).ExitCode
}

if ($Uninstall) {
    Remove-Item $shortcut -ErrorAction Ignore
    if (Test-Path $asioDll) {
        Invoke-Regsvr32 '/s', '/u', "`"$asioDll`"" | Out-Null
        Remove-Item $asioDir -Recurse -Force
    }
    $packages = pnputil /enum-drivers | Out-String
    foreach ($match in [regex]::Matches($packages, '(?ms)Published Name:\s+(oem\d+\.inf)\s+Original Name:\s+ak1acx\.inf')) {
        pnputil /delete-driver $match.Groups[1].Value /uninstall /force
    }
    Write-Output 'removed'
    return
}

$bcd = bcdedit /enum '{current}' | Out-String
if ($bcd -notmatch '(?m)^testsigning\s+Yes') {
    throw 'Test signing is off. The driver is test-signed, so first disable Secure Boot in the firmware, run "bcdedit /set testsigning on" as administrator and reboot.'
}

$cert = Get-ChildItem $here -Filter *.cer | Select-Object -First 1
if (-not $cert) { throw "no certificate (*.cer) next to $PSCommandPath" }
foreach ($store in 'Root', 'TrustedPublisher') {
    Import-Certificate -FilePath $cert.FullName -CertStoreLocation "Cert:\LocalMachine\$store" | Out-Null
}

pnputil /add-driver (Join-Path $here 'ak1acx.inf') /install
if ($LASTEXITCODE -notin 0, 259, 3010) { throw "pnputil failed with exit code $LASTEXITCODE" }

New-Item -ItemType Directory -Force $asioDir | Out-Null
Copy-Item (Join-Path $here 'ak1_asio.dll') $asioDll -Force
Copy-Item (Join-Path $here 'ak1-panel.exe') $panel -Force
$exitCode = Invoke-Regsvr32 '/s', "`"$asioDll`""
if ($exitCode) { throw "registering $asioDll failed with exit code $exitCode" }

$link = (New-Object -ComObject WScript.Shell).CreateShortcut($shortcut)
$link.TargetPath = $panel
$link.Save()

Write-Output 'installed; replug the Audio Kontrol 1 if it does not show up'
