# Roadmap

Everything below is **designed but not yet implemented** unless marked
otherwise. Ordering = impact / effort.

Recently shipped (v0.3 latency overhaul — no longer roadmap items):
independent input/video session pipelines, event-driven encode, runtime
encoder bitrate/fps (no re-init), adaptation hysteresis with sticky FPS,
single-pass BGRA→I420, TCP_NODELAY, DXGI cursor compositing, multi-monitor
input mapping, letterbox-correct touch coordinates on all viewers,
device-rate pointer sampling, latest-frame rendering everywhere.

Shipped since (v0.4 pipeline overhaul — no longer roadmap items):
zero-rebuild runtime bitrate raises (max-bitrate ceiling lift), parallel
multi-slice encode, dirty-region encoding (static-frame elision + partial
color conversion), dedicated low-latency cursor channel with client overlay
+ automatic composited fallback, per-stage latency instrumentation
(capture age / convert / encode / seal+send / arrival / decode / present),
zero-copy in-place envelope seal, immediate-paint web presentation,
MSE/fMP4 H.264 for insecure-context browsers (replacing JPEG),
hardware H.264 encoding via Media Foundation (NVENC/QuickSync/AMF),
DXGI cursor-only readback skip, profile-switch re-baselining,
QueryDisplayConfig extend-mode input mapping, multi-monitor & multi-GPU &
HDR-capable IddCx driver with CI syntax gate, reproducible benchmark
harness (`viewer/web/tests/bench.mjs`).

Shipped since (v0.5 security & features — no longer roadmap items):
**PAKE pairing** — SPAKE2 over P-256 (`auth.method = pair_pake`, default in
the web viewer / client SDK / desktop viewer) removes the offline
PIN-grinding caveat; the legacy PIN-HKDF method stays accepted for the
mobile viewers until they ship PAKE. **OS keystore for the trust store** —
DPAPI (current-user) wrapping of `devices.json` on Windows, transparent
legacy-store migration. **Clipboard sync** — permission-gated per device
(deny by default, panel toggle, live-revocable), 256 KiB cap, no-echo
fan-out to other granted viewers, host watcher on Windows
(`GetClipboardSequenceNumber` poll), full web-viewer UI (auto-receive +
Ctrl+C/Ctrl+V + toolbar send), covered by Rust + cross-stack + Chromium
E2E. **Layout-aware keyboard mapping** — viewers send `key` alongside
`code`; the Windows host picks scan-code / `VkKeyScanW` VK / Unicode
injection per event, so mismatched keyboard layouts type the right
characters. **True stylus injection** — `CreateSyntheticPointerDevice(PT_PEN)`
+ `InjectSyntheticPointerInput` with pressure/tilt/hover and automatic
mouse fallback.

## P0 — performance & the driver

1. **Driver bring-up** (needs a WDK machine): compile
   `host/windows-driver`, test-sign, validate extend mode end-to-end, measure
   ring throughput at 4K, add driver health reporting into the panel
   ("extend/mirror/pattern" badge exists server-side already).
2. **Hardware encoders — H.264 SHIPPED** (`encode/mf_h264.rs`): MFTEnumEx
   hardware enumeration (NVENC/QuickSync/AMF), async-MFT event loop at queue
   depth ≤1, MF_LOW_LATENCY + CBR + zero B-frames, runtime ICodecAPI bitrate,
   NV12 dirty-row conversion, static-frame elision, automatic software
   fallback. Compile-verified by the Windows CI job; **runtime validation
   needs a real Windows GPU machine** (this sandbox has none — see
   docs/TESTING.md release gate). Remaining: HEVC output type (trivial
   variant of the same MFT plumbing) once decoder support is negotiated.
3. **Encoder ROI from DXGI dirty/move rects**: the pixel-exact row-pair diff
   already elides static frames and limits color conversion; the remaining
   step is feeding rectangle hints into encoder rate control (needs the
   MF/NVENC encoders — OpenH264's ROI support is too limited to matter).

## P1 — security & transports

4. **PAKE for the mobile viewers**: Android (Kotlin) and iOS (Swift) still
   pair via the legacy PIN-HKDF method (their platform crypto APIs lack EC
   group arithmetic — needs a small constant-time P-256 point layer or a
   vetted library). Host/protocol/web/desktop already speak `pair_pake`;
   once mobile ships it, retire the legacy method.
5. **QUIC transport** (quinn) with the same envelopes; datagram mode for
   video, streams for control; WebTransport for the web viewer where
   available; WS stays as fallback. *Assessed 2026-07: on wired/strong-Wi-Fi
   LAN (sub-ms RTT, ~zero loss) TCP_NODELAY + latest-only send slots already
   avoid the queueing QUIC would remove, so it does not materially cut
   latency there; the win is head-of-line-blocking removal on lossy Wi-Fi.
   Kept at P1: real, but smaller than hardware encoders (P0.2). Browser
   WebTransport additionally requires TLS certs (serverCertificateHashes =
   Chromium-only today), so the web path stays WS regardless.*
6. OS keystore on non-Windows hosts + viewer-side token protection
   (Keychain / Keystore; Windows DPAPI shipped in v0.5).
7. Optional HTTPS with self-signed cert + fingerprint pinning for the web
   viewer's *code* integrity on hostile LANs.

## P2 — features

8. **Audio**: WASAPI loopback → Opus (channel 3 is reserved); per-client
   mute/volume; off by default with a visible indicator.
9. **File drop** with explicit accept dialog per transfer (clipboard sync
   shipped in v0.5; reuse its grant model).
10. **Multi-monitor / multi-client layout**: several virtual monitors (driver
    already parameterized by `MaxMonitorsSupported`), per-client monitor
    assignment, video-wall spanning mode.
11. **Gamepad forwarding** (Gamepad API → ViGEm-style injection is out of
    clean-room scope; use Windows.Gaming.Input injection when available).
12. Clipboard beyond text (images/HTML) behind the same per-device grant.

## P3 — platform breadth

13. Android/iOS CI builds (Gradle + xcodebuild GitHub runners) and store
    packaging docs.
14. Linux/macOS *hosts* (wlroots screencopy / ScreenCaptureKit) — the
    protocol and viewers are already host-OS-agnostic.
15. Opt-in remote rendezvous: end-to-end-encrypted, relay-blind (relay sees
    ciphertext only), separate binary + explicit user action; never on by
    default.

## Deliberately rejected

* Cloud accounts, telemetry, auto-update phone-home — against the product's
  core promise.
* Unsigned-driver install "hacks" — mirror mode is the honest fallback.
