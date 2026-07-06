# NebulaDisplay Virtual Display Driver (IddCx)

A UMDF v2 **Indirect Display Driver** that adds 1–4 real virtual monitors to
Windows 10 (2004+) / Windows 11 — the desktop genuinely *extends* onto them,
and the driver hands every composed frame to the `nebulad` service through
per-monitor shared-memory rings (`include/ndsp_frame_ring.h`).

> **Honest status:** this driver is complete, reviewed source code whose C++
> and API usage are **syntax/type-checked in CI on every commit** against
> stub headers modeled from Microsoft's public IddCx documentation
> (`tests/syntax-check.sh`, both the IddCx 1.10 and 1.4 header models) — but
> it has **not been compiled with a real WDK or loaded on Windows in this
> repository's CI**. Building requires Visual Studio + the Windows Driver
> Kit, and loading requires a signature (details below). Until you install
> it, `nebulad` automatically uses **mirror mode** (DXGI Desktop
> Duplication) which needs no driver at all.

## IddCx / WDK version matrix

| API | Introduced | WDK | Compile gate |
|---|---|---|---|
| `IddCxSwapChainFinishedProcessingFrame` | IddCx 1.4 (Win10 2004) | 10.0.19041 | `IDDCX_VERSION_MINOR >= 4` |
| `IDDCX_ENDPOINT_VERSION` (struct for `pFirmwareVersion` / `pHardwareVersion` — these are **not** strings) | IddCx 1.0 | any | always |
| `EvtIddCxParseMonitorDescription2` / `QueryTargetModes2` / `CommitModes2` / `AdapterQueryTargetInfo` / HDR types | IddCx 1.10 (Win11 22H2) | 10.0.22621 | `IDDCX_VERSION_MINOR >= 10` |

The vcxproj defaults to `IDDCX_VERSION_MINOR=10` (HDR-capable build) and
accepts `/p:IddcxMinor=4` for older WDKs; `IDDCX_MINIMUM_VERSION_REQUIRED=4`
keeps the runtime floor at Windows 10 2004 either way. `Driver.h` adapts with
`NDSP_IDDCX_HAS_*` feature macros — no source edits needed.

## Features

* 1–4 virtual monitors (`MonitorCount` REG_DWORD in the device hardware key;
  restart the device to apply — monitors hotplug live)
* Custom modes: `ExtraModes` REG_MULTI_SZ of `"2560x1600@75"` strings on top
  of the 14 built-ins (720p→4K, up to 120 Hz)
* Distinct EDID serial + container id per connector → Windows persists
  independent layout/scale/rotation per monitor
* Multi-GPU correct: the D3D device is created on the exact adapter the OS
  renders each monitor on (`RenderAdapterLuid`)
* HDR10/WCG capability reporting on IddCx 1.10+ hosts (8-bit + 10-bit wire
  formats, `IDDCX_TARGET_CAPS_WIDE/HIGH_COLOR_SPACE`)
* Sleep/resume safe (`EvtDeviceD0Entry` re-entry keeps the adapter),
  swap-chain reassignment on mode switches, MMCSS-boosted frame threads

## What's here

| Path | Purpose |
|---|---|
| `src/Driver.h/.cpp` | The IddCx driver: adapter/monitor lifecycle, synthesized EDIDs, mode tables, per-monitor swap-chain threads, frame-ring producers |
| `tests/syntax-check.sh` | CI syntax/type check against stub WDK headers (runs everywhere) |
| `include/ndsp_frame_ring.h` | Shared-memory ABI between driver and service (mirrored by `host/service/src/capture/windows_idd.rs`) |
| `nebuladisplay.inf` | Driver package INF (root-enumerated, `Root\NebulaDisplayIdd`) |
| `companion/devnode.cpp` | `nebula-devnode.exe` — creates/removes the software devnode via public `SwDeviceCreate` |
| `NebulaDisplayDriver.vcxproj` | WDK build project (x64 + ARM64) |

## Building (requires a Windows machine)

1. Install **Visual Studio 2022** with the *Desktop development with C++*
   workload, then the matching **Windows Driver Kit** (WDK) and the WDK VS
   extension.
2. Build:

   ```powershell
   msbuild NebulaDisplayDriver.vcxproj /p:Configuration=Release /p:Platform=x64
   cl /std:c++17 /EHsc companion\devnode.cpp /Fe:nebula-devnode.exe /link swdevice.lib onecoreuap.lib
   ```

3. The output folder contains `NebulaDisplayDriver.dll`, `nebuladisplay.inf`
   and (after `Inf2Cat`/signing) `nebuladisplay.cat`.

## Signing — read this before installing

Windows will not load an unsigned driver. Your options, honestly:

| Option | Works for | How |
|---|---|---|
| **Test signing** | Development machines | `bcdedit /set testsigning on` + self-signed cert (`installer/install-driver.ps1 -TestSign` automates it) |
| **Attestation signing** | Public distribution | Microsoft Partner Center account + EV code-signing certificate; submit the `.cab`, get back a Microsoft-signed package |
| **WHQL** | OEM/enterprise | Full HLK test pass + Partner Center submission |

There is no legitimate way around this; NebulaDisplay does not attempt one.
Mirror mode exists precisely so the product is fully usable with zero drivers.

## Installing (test-signed dev flow)

```powershell
# elevated PowerShell, from the repo root
./installer/install-driver.ps1 -DriverDir path\to\built\package -TestSign
```

which performs: enable test signing (reboot required once) → `pnputil
/add-driver nebuladisplay.inf /install` → `nebula-devnode.exe create` →
verifies the monitor appears in `Get-PnpDevice -Class Display`.

Repair/reinstall: `./installer/install-driver.ps1 -Repair`
Uninstall: `./installer/install-driver.ps1 -Uninstall`

## How frames flow

```
DWM composites the virtual monitor
  → IddCx swap chain (this driver, SwapChainProcessor thread)
  → CPU staging copy
  → Local\NebulaDisplay.FrameRing.v1 (triple-buffered seqlock ring)
  → nebulad WindowsIddSource (capture/windows_idd.rs)
  → encoder → encrypted NDSP → viewers
```

Design notes:

* **Seqlock slots** — the service can always read the newest complete frame
  without blocking the driver; a torn read is detected by the odd/even
  sequence counter and simply retried.
* **EDID** is synthesized at runtime (`BuildNdspEdid`), manufacturer ID `NBD`,
  with a correct checksum — Windows treats the monitor like real hardware, so
  per-monitor scaling/layout persist across sessions (stable container id).
* **WARP fallback** — if no hardware D3D device is available in the UMDF host,
  the swap chain runs on WARP so the virtual monitor keeps working in VMs.
* The driver never touches the network and contains no protocol code; process
  isolation boundary is the frame ring only.
