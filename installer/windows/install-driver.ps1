<#
.SYNOPSIS
  Installs (or repairs) the NebulaDisplay virtual display driver.

.DESCRIPTION
  * Optionally creates a self-signed test certificate and signs the driver
    (-TestSign) for development machines with test signing enabled.
  * Stages the driver package with pnputil.
  * Creates the root-enumerated software device Root\NebulaDisplay that the
    driver binds to (this is what makes the virtual monitor appear).

  Run from an elevated PowerShell prompt.

.PARAMETER DriverDir
  Directory containing NebulaDisplay.dll / NebulaDisplay.inf / *.cat.
  Defaults to the build output next to this script layout.

.PARAMETER TestSign
  Create/reuse a local test certificate, sign the driver, and remind about
  bcdedit test signing.

.PARAMETER Repair
  Remove the existing device+driver first, then install fresh.
#>
[CmdletBinding()]
param(
    [string]$DriverDir = (Join-Path $PSScriptRoot "..\..\host\windows-driver\x64\Release\NebulaDisplay"),
    [switch]$TestSign,
    [switch]$Repair
)

$ErrorActionPreference = "Stop"

function Assert-Admin {
    $id = [Security.Principal.WindowsIdentity]::GetCurrent()
    $principal = New-Object Security.Principal.WindowsPrincipal($id)
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw "This script must run as Administrator."
    }
}

Assert-Admin

$inf = Join-Path $DriverDir "NebulaDisplay.inf"
$dll = Join-Path $DriverDir "NebulaDisplay.dll"
if (-not (Test-Path $inf) -or -not (Test-Path $dll)) {
    throw "Driver package not found in '$DriverDir'. Build it first (see host/windows-driver/README.md)."
}

if ($Repair) {
    Write-Host ">> Repair: removing existing NebulaDisplay device/driver..." -ForegroundColor Yellow
    & "$PSScriptRoot\uninstall-driver.ps1" -Quiet
}

if ($TestSign) {
    Write-Host ">> Test-signing the driver package..." -ForegroundColor Cyan
    $certName = "NebulaDisplay Test Signing"
    $cert = Get-ChildItem Cert:\LocalMachine\My |
        Where-Object { $_.Subject -eq "CN=$certName" } | Select-Object -First 1
    if (-not $cert) {
        $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject "CN=$certName" `
            -CertStoreLocation Cert:\LocalMachine\My `
            -HashAlgorithm SHA256 -NotAfter (Get-Date).AddYears(3)
        # Trust it for driver verification on THIS machine only.
        foreach ($store in "Root", "TrustedPublisher") {
            $dst = New-Object System.Security.Cryptography.X509Certificates.X509Store($store, "LocalMachine")
            $dst.Open("ReadWrite"); $dst.Add($cert); $dst.Close()
        }
        Write-Host "   created and trusted local test certificate."
    }

    $signtool = Get-Command signtool.exe -ErrorAction SilentlyContinue
    if (-not $signtool) {
        throw "signtool.exe not on PATH — run from a Developer/EWDK prompt."
    }
    & $signtool.Source sign /fd SHA256 /sha1 $cert.Thumbprint $dll | Out-Null
    $cat = Get-ChildItem $DriverDir -Filter *.cat | Select-Object -First 1
    if ($cat) { & $signtool.Source sign /fd SHA256 /sha1 $cert.Thumbprint $cat.FullName | Out-Null }

    $ts = (bcdedit /enum "{current}" | Select-String -Quiet "testsigning\s+Yes")
    if (-not $ts) {
        Write-Warning "Test signing is OFF. Enable it and reboot before the driver will load:"
        Write-Warning "    bcdedit /set testsigning on"
    }
}

Write-Host ">> Staging driver package (pnputil)..." -ForegroundColor Cyan
pnputil /add-driver $inf /install

Write-Host ">> Creating virtual display device Root\NebulaDisplay..." -ForegroundColor Cyan
# pnputil gained /add-device only recently; fall back to devcon-style
# creation through PowerShell's PnpDevice when unavailable.
$added = $false
try {
    $out = pnputil /add-device "Root\NebulaDisplay" 2>&1
    if ($LASTEXITCODE -eq 0) { $added = $true; Write-Host $out }
} catch { }
if (-not $added) {
    $devcon = Get-Command devcon.exe -ErrorAction SilentlyContinue
    if ($devcon) {
        & $devcon.Source install $inf "Root\NebulaDisplay"
        $added = $true
    }
}
if (-not $added) {
    Write-Warning "Could not create the software device automatically."
    Write-Warning "Install 'devcon.exe' from the WDK and run: devcon install `"$inf`" Root\NebulaDisplay"
    exit 1
}

Write-Host ""
Write-Host "✔ NebulaDisplay virtual display driver installed." -ForegroundColor Green
Write-Host "  Check Settings > System > Display, and the host control panel diagnostics."
