# NebulaDisplay virtual display driver — Windows build & install

This directory contains the IddCx (Indirect Display Driver Class eXtension)
UMDF2 driver that creates NebulaDisplay's real virtual monitors.

**Status: complete source, must be compiled on Windows.** This repository's
CI/dev sandbox is Linux-based; the WDK cannot run there. Everything below is
the honest, exact path to a working driver.

## What it does

* Registers a virtual display adapter ("NebulaDisplay Virtual Adapter").
* Exposes one virtual monitor (HDMI-class) with modes from 1280×720\@60 to
  3840×2160\@60, including 1080p\@90/120 and portrait tablet modes.
* For every frame Windows composes on that monitor, copies the swap-chain
  buffer into a named shared-memory section
  (`Global\NebulaDisplay.Frame.0`) and pulses
  `Global\NebulaDisplay.FrameReady.0`. The `nebula-host` service picks
  frames up from there (`--source virtual`, used automatically for
  extend-mode sessions).

## Build (Windows 10/11 x64)

1. Install **Visual Studio 2022** (Desktop C++ workload) and the matching
   **Windows Driver Kit** — or download the standalone **EWDK** ISO which
   needs no installation.
2. From a *Developer/EWDK command prompt*:

   ```bat
   cd host\windows-driver
   msbuild NebulaDisplay.vcxproj /p:Configuration=Release /p:Platform=x64
   ```

   Output: `x64\Release\NebulaDisplay\` containing `NebulaDisplay.dll`,
   `NebulaDisplay.inf`, and `nebuladisplay.cat`.

## Signing — read this honestly

Windows requires signed drivers:

| Scenario | What you need |
|---|---|
| **Local development** | Enable test signing: `bcdedit /set testsigning on` + reboot, then sign with a self-signed test cert (`installer/windows/install-driver.ps1 -TestSign` does this for you). |
| **Distribution to other machines** | An EV code-signing certificate **and** Microsoft **Hardware Dev Center attestation signing** of the `.cab`. There is no way around this; budget for the EV cert and a Partner Center account. |

Until attestation-signed, other users' machines will refuse the driver
unless they enable test signing themselves. NebulaDisplay therefore always
supports **capture-only mirror mode** (DXGI Desktop Duplication) with zero
driver installation — extend mode is the only feature that needs the driver.

## Install / repair / uninstall

Run PowerShell **as Administrator**:

```powershell
# install (test-sign + create the root-enumerated device):
installer\windows\install-driver.ps1 -TestSign

# verify:
pnputil /enum-devices /class Display | findstr /i nebula

# repair (reinstall in place):
installer\windows\install-driver.ps1 -Repair

# uninstall completely:
installer\windows\uninstall-driver.ps1
```

After installation, a new display appears in **Settings → System → Display**
whenever the monitor is active, and `nebula-host` diagnostics report
`virtual_display_driver: running`.

## Architecture notes

* UMDF2 user-mode driver: a crash cannot blue-screen the machine — the
  reflector restarts the driver host process and IddCx re-arrives the
  monitor. The host service detects the section disappearing and reconnects.
* The shared-memory export was chosen over custom IOCTLs deliberately: it is
  a minimal, auditable surface (fixed-size section + one event), and the
  torn-frame check (sequence number before/after copy) keeps it correct
  without cross-process locks.
* Frame-rate pacing is inherited from Windows' own composition cadence for
  the selected mode; the service's adaptive controller decides what to
  encode and send.
