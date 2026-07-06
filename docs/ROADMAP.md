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

4. **PAKE pairing** (CPace or SPAKE2) replacing PIN-bound HKDF: removes the
   offline-grinding caveat in SECURITY.md; wire format has room (new
   `auth.method`).
5. **QUIC transport** (quinn) with the same envelopes; datagram mode for
   video, streams for control; WebTransport for the web viewer where
   available; WS stays as fallback. *Assessed 2026-07: on wired/strong-Wi-Fi
   LAN (sub-ms RTT, ~zero loss) TCP_NODELAY + latest-only send slots already
   avoid the queueing QUIC would remove, so it does not materially cut
   latency there; the win is head-of-line-blocking removal on lossy Wi-Fi.
   Kept at P1: real, but smaller than hardware encoders (P0.2). Browser
   WebTransport additionally requires TLS certs (serverCertificateHashes =
   Chromium-only today), so the web path stays WS regardless.*
6. OS keystore for trust tokens (DPAPI / Keychain / Keystore).
7. Optional HTTPS with self-signed cert + fingerprint pinning for the web
   viewer's *code* integrity on hostile LANs.

## P2 — features

8. **Audio**: WASAPI loopback → Opus (channel 3 is reserved); per-client
   mute/volume; off by default with a visible indicator.
9. **Clipboard sync** with explicit per-device permission + per-event size
   caps (protocol slot: control messages).
10. **File drop** with explicit accept dialog per transfer.
11. **Multi-monitor / multi-client layout**: several virtual monitors (driver
    already parameterized by `MaxMonitorsSupported`), per-client monitor
    assignment, video-wall spanning mode.
12. **Gamepad forwarding** (Gamepad API → ViGEm-style injection is out of
    clean-room scope; use Windows.Gaming.Input injection when available).
13. Layout-aware keyboard mapping (send both `code` and `key`, host picks).
14. Stylus: Windows Ink `InjectSyntheticPointerInput` for true pressure/tilt
    (current fallback maps pen to mouse).

## P3 — platform breadth

15. Android/iOS CI builds (Gradle + xcodebuild GitHub runners) and store
    packaging docs.
16. Linux/macOS *hosts* (wlroots screencopy / ScreenCaptureKit) — the
    protocol and viewers are already host-OS-agnostic.
17. Opt-in remote rendezvous: end-to-end-encrypted, relay-blind (relay sees
    ciphertext only), separate binary + explicit user action; never on by
    default.

## Deliberately rejected

* Cloud accounts, telemetry, auto-update phone-home — against the product's
  core promise.
* Unsigned-driver install "hacks" — mirror mode is the honest fallback.
