# 🌌 NebulaDisplay

**Turn any device into an extra monitor for your Windows PC — private,
encrypted, LAN-only, no accounts, no cloud.**

NebulaDisplay is an original, clean-room virtual-monitor and remote-display
suite. A Windows PC runs the **host**; browsers, Android/iOS devices, and
desktop apps are **viewers** that become extra displays (extend) or mirrors
of existing ones.

```
┌────────────────────────── Windows host ──────────────────────────┐
│  IddCx virtual display driver ──┐                                │
│  (real extra monitors)          ├─► capture ─► dirty-rect        │
│  DXGI desktop duplication ──────┘    detect     JPEG/H.264*      │
│                                              │                   │
│  WASAPI loopback audio ──► PCM/Opus* ────────┤                   │
│                                              ▼                   │
│  pairing · trust store · adaptive control · TLS WebSocket (NDSP) │
└────────────────────────────────┬─────────────────────────────────┘
              LAN / Wi-Fi / USB-tether (no internet needed)
      ┌───────────┬──────────────┼──────────────┬───────────────┐
      ▼           ▼              ▼              ▼               ▼
  Web viewer  Windows/macOS  Android app    iPadOS app     (your next
  (no install)  /Linux app   (touch)      (Apple Pencil)     device)
```
\* H.264/Opus are wired in the protocol; MJPEG/PCM are the shipping codecs (see roadmap).

## Highlights

* **Real virtual monitors** on Windows via an IddCx indirect display driver
  (extend mode), with an automatic **zero-driver fallback** (mirror mode via
  DXGI Desktop Duplication) when the driver isn't installed.
* **Web viewer with no install** — open a URL, enter a PIN, done.
* **Encrypted by default** (TLS with certificate-fingerprint pinning) and
  **secure pairing** (single-use 6-digit PIN + QR, per-device revocable
  tokens, rate-limited).
* **Input is opt-in per device**: mouse/keyboard/touch/stylus injection is
  disabled until the host user flips the switch for that device.
* **Adaptive quality**: AIMD congestion control from socket backpressure,
  client decode feedback, and RTT bufferbloat detection; profiles for
  Office / Video / Drawing / Gaming.
* **Dirty-region streaming**: only changed screen areas are encoded and
  sent — 10–100× bandwidth reduction on desktop workloads.
* **Diagnostics everywhere**: FPS/latency/bitrate overlay in the viewer,
  full per-client health on the host control panel, honest driver status.
* **No telemetry. No account. Local only.** There is nothing to opt out of.

## Quick start (development)

```bash
# host (any OS for development; Windows for real capture)
cd viewer/web && npm install && npm run build && cd ../..
cargo run -p nebula-host -- --source test        # test pattern on non-Windows

# open the control panel
#   https://localhost:38470/         (accept the self-signed cert once)
# open the viewer from another device
#   https://<host-ip>:38470/view/
# click "Pair a device" on the panel → enter the PIN on the viewer.
```

On Windows, `--source auto` (default) mirrors your primary monitor;
installing the driver (`installer/windows/install-driver.ps1`) enables real
extend mode. See [docs/BUILD.md](docs/BUILD.md) and
[host/windows-driver/README.md](host/windows-driver/README.md).

## Repository layout

| Path | What |
|---|---|
| `crates/nebula-proto` | NDSP protocol: messages, binary packets, negotiation (Rust) |
| `crates/nebula-host` | Host service: capture, encode, pairing, streaming server, discovery, input, audio |
| `host/windows-driver` | IddCx UMDF2 virtual display driver (C++) |
| `host/tray-ui` | Win32 tray companion |
| `viewer/web` | Browser viewer + host control panel (TypeScript) |
| `viewer/desktop` | Tauri desktop viewer shell |
| `viewer/android` | Android viewer (Kotlin) |
| `viewer/ios` | iOS/iPadOS viewer (Swift) |
| `installer/windows` | Inno Setup installer + driver install/repair scripts |
| `docs/` | Architecture, protocol, security, testing, troubleshooting |
| `tests/` | Browser smoke test (Playwright) & artifacts |

## Documentation

* [Architecture](docs/ARCHITECTURE.md) · [Protocol (NDSP)](docs/PROTOCOL.md)
* [Security & threat model](docs/SECURITY.md)
* [Building](docs/BUILD.md) · [Testing & test matrix](docs/TESTING.md)
* [Driver install & signing](host/windows-driver/README.md)
* [Troubleshooting](docs/TROUBLESHOOTING.md)
* [Release checklist](docs/RELEASE_CHECKLIST.md)

## Clean-room statement

NebulaDisplay is an original work: its name, protocol (NDSP), architecture,
code, UI, and docs were created from scratch against public OS APIs and
official documentation (IddCx, DXGI, WASAPI, W3C). It does not contain or
derive from any competitor's code, protocol, or assets.

## License

Apache-2.0 — see [LICENSE](LICENSE).
