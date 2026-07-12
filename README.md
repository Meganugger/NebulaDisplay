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
- **Encrypted by default**: SPAKE2 (PAKE) PIN pairing → AES-256-GCM on every
  frame — recorded handshakes can't be PIN-ground offline; per-device trust
  tokens; input **and clipboard denied until you allow them**; optional
  HTTPS with a pinnable self-signed certificate.
- **Adaptive**: AIMD bitrate/FPS driven by real congestion signals; profiles
  for Office / Video / Drawing / Gaming.
- **Audio, opt-in twice**: WASAPI loopback → Opus, off by default on the
  host *and* per viewer, with a live 🔊 indicator in the panel.
- **Clipboard sync** (text) with per-device permission and size caps; true
  stylus injection (pressure/tilt) and layout-aware typing on Windows.
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

Verified by automated tests (70 Rust tests + Node compat on two crypto
backends + full Chromium E2E in CI): protocol/crypto, SPAKE2 + legacy
pairing, trust/grants (input & clipboard), H.264+JPEG streaming, Opus audio
(opt-in, decodable packets), clipboard permission/size gating, HTTPS with
certificate pinning, adaptation, discovery, panel. Written and
compile-verified for Windows but **needing a Windows/WDK/SDK machine to
build & runtime-validate**: the IddCx driver (extend mode), DXGI
mirror/SendInput/pen/clipboard/WASAPI runtime behavior, MF hardware
encoders, tray app runtime, Android/iOS apps. Not implemented yet (designed,
reserved in the protocol): file drop, QUIC, multi-monitor layout — see
[ROADMAP](docs/ROADMAP.md).

## Clean-room statement

NebulaDisplay is an original work: its protocol, code, UI, and docs were
written from scratch against public OS APIs and public documentation only. It
is *functionally comparable* to commercial products in the category but
derives nothing from them.

## License

MIT — see [LICENSE](LICENSE).
