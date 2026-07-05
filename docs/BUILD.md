# Building NebulaDisplay

## Prerequisites

| Component | Needs |
|---|---|
| `nebula-host`, `nebula-proto` | Rust 1.80+ (any OS) |
| Web viewer / control panel | Node 20+ |
| Windows driver | Windows + Visual Studio 2022 + WDK (or EWDK) |
| Tray app | Windows + any VS Developer prompt |
| Installer | Windows + Inno Setup 6 |
| Android viewer | Android Studio / SDK 35 |
| iOS viewer | macOS + Xcode 15 |

## Host service + web UI (all platforms)

```bash
# 1. web UI (the host serves it)
cd viewer/web
npm install
npm run build            # tsc --noEmit + vite build → dist/

# 2. host
cd ../..
cargo build --release -p nebula-host

# 3. run
target/release/nebula-host                  # Windows: mirrors monitor 0
target/release/nebula-host --source test    # any OS: synthetic pattern
```

Useful flags: `--port`, `--bind`, `--name`, `--no-tls` (loopback testing
only), `--web-dir <dist>`, `--config <file>`, `--print-access`.

### Cross-checking Windows code from Linux/macOS

All Windows-only code (DXGI, SendInput, WASAPI, service wrapper, IddCx
bridge) type-checks without a Windows machine:

```bash
rustup target add x86_64-pc-windows-msvc
cargo check --target x86_64-pc-windows-msvc --no-default-features
```

(`--no-default-features` skips the TLS C dependencies that can't
cross-compile without a Windows linker; native builds keep TLS on.)

## Tests

```bash
cargo test                                   # unit + integration (41 tests)
cd viewer/web && npm run typecheck           # strict TS
node tests/browser-smoke.mjs                 # full browser E2E (Playwright)
```

The browser smoke test boots the real host + real viewer in headless
Chromium, pairs with a PIN, verifies frames animate on the canvas, checks
the stats overlay and token-based reconnect, and saves screenshots to
`tests/artifacts/`.

## Windows driver

See [host/windows-driver/README.md](../host/windows-driver/README.md) —
build with `msbuild NebulaDisplay.vcxproj /p:Configuration=Release
/p:Platform=x64` from an EWDK prompt, then
`installer/windows/install-driver.ps1 -TestSign`.

## Tray app

```bat
cd host\tray-ui && build.bat
```

## Installer

```bat
iscc installer\windows\nebuladisplay.iss
```

Inputs it packages: release host exe, `viewer/web/dist`, tray exe, and the
driver package if present. It registers the `NebulaDisplayHost` service,
adds scoped firewall rules, and (optionally) installs the driver.

## Portable viewer

The web viewer *is* the portable, admin-free viewer — any browser, no
install. For a native portable viewer, `viewer/desktop` builds a single
self-contained binary (see its README).
