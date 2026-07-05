<#
.SYNOPSIS
  Removes the NebulaDisplay virtual display device and driver package.
#>
[CmdletBinding()]
param([switch]$Quiet)

$ErrorActionPreference = "Continue"

if (-not $Quiet) { Write-Host ">> Removing NebulaDisplay devices..." -ForegroundColor Cyan }

# Remove the root-enumerated device(s).
Get-PnpDevice -Class Display -ErrorAction SilentlyContinue |
    Where-Object { $_.InstanceId -like "ROOT\NEBULADISPLAY*" -or $_.FriendlyName -like "*NebulaDisplay*" } |
    ForEach-Object {
        if (-not $Quiet) { Write-Host "   removing $($_.InstanceId)" }
        pnputil /remove-device $_.InstanceId | Out-Null
    }

# Remove the staged driver package(s).
pnputil /enum-drivers |
    Select-String -Context 0, 6 "nebuladisplay.inf" |
    ForEach-Object {
        $published = ($_ | Select-String "Published Name" -SimpleMatch)
    }

# Simpler robust approach: parse enum-drivers output for our INF.
$lines = pnputil /enum-drivers
$current = $null
foreach ($line in $lines) {
    if ($line -match "Published Name\s*:\s*(oem\d+\.inf)") { $current = $Matches[1] }
    if ($line -match "Original Name\s*:\s*nebuladisplay\.inf" -and $current) {
        if (-not $Quiet) { Write-Host "   deleting driver package $current" }
        pnputil /delete-driver $current /uninstall /force | Out-Null
        $current = $null
    }
}

if (-not $Quiet) { Write-Host "✔ NebulaDisplay driver removed." -ForegroundColor Green }
