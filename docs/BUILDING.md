# Building NebulaDisplay

## Prerequisites

| Component | Needs |
|---|---|
| Host service, protocol, desktop viewer, tray | Rust 1.80+ (`rustup`), CMake (libopus build — preinstalled on GitHub runners; `apt install cmake` / `brew install cmake` / VS Build Tools otherwise) |
| Web viewer + panel | Node 22+ / npm |
| Windows virtual display driver | Windows + VS2022 + WDK (see `host/windows-driver/README.md`) |
| Android viewer | Android SDK 35, JDK 17 (see `viewer/android/README.md`) |
| iOS viewer | Xcode 15+ on macOS (see `viewer/ios/README.md`) |

Everything in the first two rows builds and tests on Linux/macOS/Windows.

## Quick start (host + web viewer)

```bash
# 1. web viewer (once, and after UI changes)
cd viewer/web && npm install && npm run build && cd ../..

# 2. host
cargo run --release -p nebulad
```

`nebulad` prints the viewer URLs and the pairing PIN; the control panel is at
`http://127.0.0.1:41888/panel.html`. On Windows it captures the real desktop
(mirror mode, or extend mode if the driver is installed); elsewhere or with
`--test-pattern` it streams a synthetic animated screen.

Useful flags: `--port`, `--panel-port`, `--discovery-port 0` (disable
discovery), `--bind`, `--name`, `--data-dir`, `--web-dir`, `--capture-size`,
`--test-pattern`.

Feature flags: `--no-default-features` drops the OpenH264 encoder (JPEG-only,
much faster cold build).

## Desktop viewer

```bash
cargo run --release -p nebula-viewer -- --host 192.168.1.20:41800 --pin 123456
# afterwards (trusted):  cargo run --release -p nebula-viewer -- --host 192.168.1.20:41800
```

## Tests & checks (what CI runs)

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace                     # unit + protocol + socket e2e
cd viewer/web
npm run build                              # strict tsc + vite
node tests/web-compat.mjs                  # web crypto vs Rust host, real WS
node tests/browser-e2e.mjs                 # full Chromium E2E (needs playwright chromium)
```

## Windows packaging

```powershell
cargo build --release
cd viewer/web; npm install; npm run build; cd ../..
powershell installer/make-portable.ps1     # portable zip, no admin
iscc installer/nebuladisplay.iss           # installer (Inno Setup 6)
```

Driver build/sign/install: `host/windows-driver/README.md` +
`installer/install-driver.ps1`.
