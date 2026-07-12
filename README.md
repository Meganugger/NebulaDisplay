# 🌌 NebulaDisplay

**Turn any device into an extra monitor for your Windows PC — private,
encrypted, local-only.** A clean-room virtual monitor & remote display suite:
Windows IddCx virtual displays streamed to browser / Android / iOS / desktop
viewers over an original encrypted protocol (**NDSP**). No cloud, no accounts,
no telemetry.

```
Windows PC (host)                            any device (viewer)
┌───────────────────┐   encrypted NDSP    ┌──────────────────────┐
│ nebulad           │ ═══════════════════▶│ browser (no install) │
│  extend: IddCx    │   H.264 / JPEG      │ Windows/macOS/Linux  │
│  mirror: DXGI     │ ◀═══════════════════│ Android · iOS        │
│  input ⇦ grants   │   touch/pen/keys    └──────────────────────┘
└───────────────────┘
```

## Highlights

- **Real virtual monitors** (Windows extend mode) via an original IddCx
  driver — plus a zero-driver **mirror mode** that works out of the box.
- **Web viewer with no install**: WebCodecs H.264 decode, touch/pen/keyboard,
  stats overlay with *measured* end-to-end latency.
- **Encrypted by default**: ECDH P-256 + a real **PAKE** (PIN pairing whose
  transcript can't be brute-forced offline) → AES-256-GCM on every frame;
  per-device trust tokens (DPAPI-protected at rest on Windows); input,
  clipboard and file transfers **denied until you allow them**.
- **Adaptive**: AIMD bitrate/FPS driven by real congestion signals; profiles
  for Office / Video / Drawing / Gaming; hardware H.264/HEVC when available.
- **More than pixels**: opt-in host audio (Opus), permission-gated clipboard
  sync and drag-&-drop file transfer (explicit accept per file), pen with
  pressure/tilt via Windows Ink, layout-aware keyboard, gamepad forwarding.
- **Local-first**: LAN, hotspot, or USB (`adb reverse`) — internet never
  required, nothing phones home.
- Control panel with QR pairing, live client stats, per-device input grants,
  one-click revocation.

## Quick start

```bash
# 1. build the web viewer (once)
cd viewer/web && npm install && npm run build && cd ../..

# 2. run the host (Windows mirrors your desktop; other OSes stream a test pattern)
cargo run --release -p nebulad
```

Open the printed URL on the other device, enter the PIN — done. Panel:
`http://127.0.0.1:41888/panel.html`. Extend mode (true extra monitor):
[host/windows-driver/README.md](host/windows-driver/README.md).

## Configuration

`config.toml` lives in the data dir (`%APPDATA%\NebulaDisplay` /
`~/.config/nebuladisplay`). Notable knobs (all default-off / safe):

| Key | Default | Meaning |
|---|---|---|
| `audio` | `false` | Stream host audio (WASAPI loopback → Opus) to sessions that opt in; panel shows a live indicator |
| `require_pake` | `false` | Refuse legacy (pre-PAKE) PIN pairing once all your viewers are updated |
| `https` | `false` | Serve the viewer over TLS with a persisted self-signed cert (fingerprint printed) |
| `clipboard_max_bytes` | `262144` | Per-payload clipboard sync cap |
| `file_max_bytes` | `2147483648` | Per-file drop cap; accepted files land in `<data_dir>/downloads` (`file_dir` overrides) |
| `pin_digits` / `pin_ttl_secs` / `max_pin_attempts` / `lockout_secs` | `6/300/5/300` | Pairing PIN policy |
| `max_fps` | `60` | Global FPS cap on top of profiles |

Clipboard and file-drop access are additionally **per-device grants** in the
panel, exactly like input — deny by default, live-revocable.

## Repository layout

| Path | What |
|---|---|
| [`shared/protocol`](shared/protocol) | NDSP v1 wire format + crypto (Rust, the authority) |
| [`shared/client`](shared/client) | Client SDK (pairing/reconnect/session) |
| [`host/service`](host/service) | `nebulad` — capture, encode, encrypt, stream, input, panel |
| [`host/windows-driver`](host/windows-driver) | IddCx virtual display driver (C++) |
| [`host/tray-ui`](host/tray-ui) | Windows tray companion |
| [`viewer/web`](viewer/web) | Browser viewer + control panel (TypeScript) |
| [`viewer/desktop`](viewer/desktop) | Native portable viewer (Rust) |
| [`viewer/android`](viewer/android) · [`viewer/ios`](viewer/ios) | Mobile viewers (Kotlin / Swift) |
| [`installer`](installer) | Inno Setup installer, portable bundle, driver install scripts |
| [`docs`](docs) | [Architecture](docs/ARCHITECTURE.md) · [Protocol](docs/PROTOCOL.md) · [Security](docs/SECURITY.md) · [Building](docs/BUILDING.md) · [Testing](docs/TESTING.md) · [Browser compat](docs/BROWSER-COMPAT.md) · [Connectivity](docs/CONNECTIVITY.md) · [Troubleshooting](docs/TROUBLESHOOTING.md) · [Roadmap](docs/ROADMAP.md) |

## Status (honest)

Verified by automated tests (70+ Rust tests + Node compat + full Chromium
E2E in CI): protocol/crypto, **PAKE pairing** (cross-implementation vectors
Rust ↔ web), trust/grants, H.264+JPEG streaming, adaptation, discovery,
panel, **clipboard sync**, **file drop** (explicit accept + SHA-256),
**audio** (Opus over channel 3, test-tone source in CI), optional **HTTPS**.
Written and compile-verified against the msvc target but **needing a real
Windows machine for runtime validation**: the IddCx driver (extend mode),
DXGI mirror/SendInput, WASAPI loopback, Win32 clipboard, Windows Ink pen,
gamepad injection, DPAPI stores, hardware H.264/HEVC MFTs, tray app,
Android/iOS apps. See [ROADMAP](docs/ROADMAP.md) for what remains.

## Clean-room statement

NebulaDisplay is an original work: its protocol, code, UI, and docs were
written from scratch against public OS APIs and public documentation only. It
is *functionally comparable* to commercial products in the category but
derives nothing from them.

## License

MIT — see [LICENSE](LICENSE).
