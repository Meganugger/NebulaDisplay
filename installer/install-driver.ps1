<#
.SYNOPSIS
  Install, repair or remove the NebulaDisplay virtual display driver.

.DESCRIPTION
  Automates the full IddCx driver lifecycle on a dev/test machine:
    install  : (optionally) enable test signing, stage the driver package with
               pnputil, create the software devnode, verify the monitor exists
    repair   : remove + re-stage the package and recreate the devnode
    uninstall: remove devnode + delete the driver package
  Production machines should install an attestation-signed package instead —
  see host/windows-driver/README.md. NebulaDisplay works without this driver
  (mirror mode); the driver only adds true "extend" mode.

.EXAMPLE
  ./install-driver.ps1 -DriverDir .\driver-package -TestSign
  ./install-driver.ps1 -Repair
  ./install-driver.ps1 -Uninstall
#>
[CmdletBinding(DefaultParameterSetName = "Install")]
param(
    [Parameter(ParameterSetName = "Install")]
    [string]$DriverDir = "$PSScriptRoot\..\host\windows-driver\x64\Release",

    [Parameter(ParameterSetName = "Install")]
    [switch]$TestSign,

    [Parameter(ParameterSetName = "Repair")]
    [switch]$Repair,

    [Parameter(ParameterSetName = "Uninstall")]
    [switch]$Uninstall
)

$ErrorActionPreference = "Stop"
$InfName = "nebuladisplay.inf"
$HardwareId = "Root\NebulaDisplayIdd"

function Assert-Admin {
    $principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw "Run this script from an elevated (Administrator) PowerShell."
    }
}

function Get-DevNodeExe {
    foreach ($p in @("$PSScriptRoot\nebula-devnode.exe",
                     "$DriverDir\nebula-devnode.exe",
                     "$PSScriptRoot\..\host\windows-driver\companion\nebula-devnode.exe")) {
        if (Test-Path $p) { return $p }
    }
    throw "nebula-devnode.exe not found — build host/windows-driver/companion/devnode.cpp first."
}

function Get-StagedPackage {
    (pnputil /enum-drivers | Select-String -Context 0, 8 "$InfName") 2>$null
}

function Remove-StagedPackages {
    $out = pnputil /enum-drivers
    $current = $null
    foreach ($line in $out) {
        if ($line -match "Published Name:\s+(oem\d+\.inf)") { $current = $Matches[1] }
        if ($line -match [regex]::Escape($InfName) -and $current) {
            Write-Host "Removing staged package $current"
            pnputil /delete-driver $current /uninstall /force | Out-Null
            $current = $null
        }
    }
}

function Install-Driver {
    Assert-Admin
    $inf = Join-Path $DriverDir $InfName
    if (-not (Test-Path $inf)) {
        throw "Driver package not found at $inf — build the driver first (host/windows-driver/README.md)."
    }

    if ($TestSign) {
        $ts = (bcdedit /enum "{current}" | Select-String "testsigning\s+Yes")
        if (-not $ts) {
            Write-Warning "Enabling TESTSIGNING boot mode. A reboot is required before the driver can load."
            bcdedit /set testsigning on | Out-Null
            Write-Host "Re-run this script after rebooting." -ForegroundColor Yellow
        }
        $cat = Join-Path $DriverDir "nebuladisplay.cat"
        if (-not (Test-Path $cat)) {
            Write-Warning "No catalog found — creating a self-signed test certificate and signing the package."
            $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject "CN=NebulaDisplay Test" `
                -CertStoreLocation Cert:\LocalMachine\My
            Export-Certificate -Cert $cert -FilePath "$env:TEMP\ndsp-test.cer" | Out-Null
            Import-Certificate -FilePath "$env:TEMP\ndsp-test.cer" -CertStoreLocation Cert:\LocalMachine\Root | Out-Null
            Import-Certificate -FilePath "$env:TEMP\ndsp-test.cer" -CertStoreLocation Cert:\LocalMachine\TrustedPublisher | Out-Null
            & "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\inf2cat.exe" /driver:$DriverDir /os:10_x64
            & "${env:ProgramFiles(x86)}\Windows Kits\10\bin\*\x64\signtool.exe" sign /fd SHA256 /sm /s My /n "NebulaDisplay Test" $cat
        }
    }

    Write-Host "Staging driver package…"
    pnputil /add-driver $inf /install
    if ($LASTEXITCODE -ne 0) { throw "pnputil failed ($LASTEXITCODE) — is the package signed? (-TestSign)" }

    Write-Host "Creating virtual display devnode…"
    & (Get-DevNodeExe) create
    if ($LASTEXITCODE -ne 0) { throw "devnode creation failed" }

    Start-Sleep -Seconds 2
    $dev = Get-PnpDevice -Class Display -ErrorAction SilentlyContinue |
        Where-Object { $_.InstanceId -like "*NebulaDisplayIdd*" }
    if ($dev -and $dev.Status -eq "OK") {
        Write-Host "SUCCESS: NebulaDisplay virtual monitor is active." -ForegroundColor Green
        Write-Host "Start nebulad — it will switch from mirror to extend mode automatically."
    } else {
        Write-Warning "Devnode exists but the monitor is not OK yet (status: $($dev.Status)). Check Device Manager → Display adapters, and Event Viewer → DriverFrameworks-UserMode."
    }
}

function Uninstall-Driver {
    Assert-Admin
    Write-Host "Removing devnode…"
    & (Get-DevNodeExe) remove
    Remove-StagedPackages
    Write-Host "Done. (Test-signing boot mode, if enabled, was left on: 'bcdedit /set testsigning off' to disable.)"
}

if ($Uninstall) { Uninstall-Driver }
elseif ($Repair) { Assert-Admin; & (Get-DevNodeExe) remove; Remove-StagedPackages; Install-Driver }
else { Install-Driver }
