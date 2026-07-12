# NebulaDisplay iOS/iPadOS viewer

Native SwiftUI client speaking NDSP v1: `URLSessionWebSocketTask` transport,
CryptoKit pairing (P-256 ECDH + HKDF + AES-256-GCM, byte-compatible with the
Rust host), VideoToolbox hardware H.264 decode (Annex-B → AVCC conversion,
SPS/PPS harvesting), JPEG fallback, touch forwarding, per-host trust tokens.

> **Note:** this app still pairs via the legacy PIN-HKDF handshake (hosts accept it by default; `allow_legacy_pairing = false` refuses it). Porting NDSP-PAKE v1 here is tracked in docs/ROADMAP.md (P1).


> **Honest status:** complete source, **not compiled in this repo's CI** —
> building requires Xcode on macOS. The protocol/crypto flow is identical to
> the verified Rust/web clients; expect normal first-build fixes only.

## Building

1. On macOS with Xcode 15+: *File → New → Project → iOS App* named
   `NebulaViewer`, bundle id of your choosing, interface SwiftUI.
2. Delete the template `ContentView.swift`/`App.swift` and add the three files
   from `NebulaViewer/` here.
3. In *Signing & Capabilities* pick your team (free personal team works for
   sideloading to your own device).
4. Info.plist: add `NSLocalNetworkUsageDescription` ("Connects to your PC on
   the local network to display its screen") — iOS 14+ local-network prompt.
5. Run on a device (simulator works too; VideoToolbox falls back to software).

## Connectivity

* **Wi-Fi**: enter `host-ip:41800` + PIN from the host panel.
* **USB (wired)**: connect the cable and enable *Personal Hotspot* or use the
  Ethernet-over-USB interface exposed by iTunes/Apple Mobile Device Service —
  the PC appears on a `172.20.10.x`-style subnet; use the PC's address on that
  interface. A usbmuxd-based transport (no hotspot required) is designed in
  docs/ROADMAP.md.
* Triple-tap the stream to disconnect.
