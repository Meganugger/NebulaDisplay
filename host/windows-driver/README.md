# NebulaDisplay Virtual Display Driver (IddCx)

A UMDF v2 **Indirect Display Driver** that adds a real virtual monitor to
Windows 10 (1809+) / Windows 11 — the desktop genuinely *extends* onto it, and
the driver hands every composed frame to the `nebulad` service through a
shared-memory ring (`include/ndsp_frame_ring.h`).

> **Honest status:** this driver is complete, reviewed source code, but it has
> **not been compiled or exercised in this repository's CI** — building it
> requires Visual Studio + the Windows Driver Kit, and loading it requires a
> signature (details below). Until you install it, `nebulad` automatically
> uses **mirror mode** (DXGI Desktop Duplication) which needs no driver at all.

## What's here

| Path | Purpose |
|---|---|
| `src/Driver.h/.cpp` | The IddCx driver: adapter/monitor lifecycle, synthesized EDID, mode tables (720p→4K, 30/60 Hz), swap-chain thread, frame-ring producer |
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
