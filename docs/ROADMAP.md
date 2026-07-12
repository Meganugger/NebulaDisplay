# Roadmap

Everything below is **designed but not yet implemented** unless marked
otherwise. Ordering = impact / effort.

Recently shipped (v0.3 latency overhaul — no longer roadmap items):
independent input/video session pipelines, event-driven encode, runtime
encoder bitrate/fps (no re-init), adaptation hysteresis with sticky FPS,
single-pass BGRA→I420, TCP_NODELAY, DXGI cursor compositing, multi-monitor
input mapping, letterbox-correct touch coordinates on all viewers,
device-rate pointer sampling, latest-frame rendering everywhere.

Shipped in v0.4 (pipeline overhaul — no longer roadmap items):
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

Shipped in v0.5 (security & features wave — no longer roadmap items):

* **PAKE pairing** (was P1.4): CPace-pattern balanced PAKE over
  ristretto255 replaces the offline-grindable PIN-in-HKDF construction.
  Cross-implementation-tested byte-identical between the Rust host
  (curve25519-dalek) and the web viewer (@noble/curves); no silent
  downgrade; `require_pake` config to refuse legacy clients.
* **Clipboard sync** (was P2.9): per-device grant (deny by default, panel
  toggle, live-revocable), size caps both directions, origin-tagged
  broadcast (no echo loops); real Win32 clipboard backend + in-memory
  test backend so CI exercises the whole path.
* **File drop** (was P2.10): explicit per-transfer accept in the host
  panel, chunked over new encrypted channel 4 with backpressure, name
  sanitization, size caps, collision-safe writes and full SHA-256
  verification before the file lands.
* **Audio** (was P2.8): WASAPI loopback → Opus (48 kHz stereo, 20 ms) on
  encrypted channel 3; off by default, per-session opt-in, panel
  indicator; test-tone source on non-Windows so e2e covers the pipeline;
  web playback via AudioDecoder + WebAudio with mute/volume.
* **Layout-aware keyboard** (was P2.13): viewers send `code` + `key`; the
  Windows host injects by scancode when layouts agree, translates via
  `VkKeyScanW` with shift compensation on mismatch, and falls back to
  Unicode injection.
* **Stylus** (was P2.14): true Windows Ink pen injection
  (`InjectSyntheticPointerInput`) with pressure/tilt; automatic fallback
  to the old pen→mouse mapping when unavailable.
* **Gamepad forwarding** (was P2.12): web Gamepad API (standard mapping)
  → `Windows.UI.Input.Preview.Injection`. Kernel-level (ViGEm-style)
  injection stays out of clean-room scope, so raw-XInput-only games won't
  see it — Windows.Gaming.Input/GameInput consumers do.
* **OS keystore** (was P1.6, host+desktop part): trust stores are
  DPAPI-protected at rest on Windows (`ndsp-keystore` crate), transparent
  migration from plaintext files. Mobile Keychain/Keystore still open.
* **Optional HTTPS** (was P1.7): `https = true` serves the viewer over TLS
  with a persisted self-signed cert (SHA-256 fingerprint printed + shown
  in the panel), closing the viewer-code-tampering hole and giving
  browsers a secure context.
* **Hardware HEVC** (was the P0.2 remainder): the Media Foundation
  encoder now emits HEVC when the client prefers it and a hardware MFT
  exists (Annex-B, IRAP keyframe detection); web viewer probes WebCodecs
  HEVC decode and prefers it over H.264.

## P0 — needs real hardware to close out

1. **Driver bring-up** (needs a WDK machine): compile
   `host/windows-driver`, test-sign, validate extend mode end-to-end, measure
   ring throughput at 4K, add driver health reporting into the panel
   ("extend/mirror/pattern" badge exists server-side already).
2. **Windows runtime validation pass** for the v0.5 features implemented in
   this sandboxed workspace: WASAPI loopback formats across devices, Win32
   clipboard under contention, Windows Ink injection into major drawing
   apps, InputInjector gamepad availability, DPAPI migration, MF HEVC
   output on NVENC/QuickSync/AMF. All compile-verified against the msvc
   target + covered by cross-platform tests; none of it has run on a
   physical Windows GPU machine yet (see docs/TESTING.md release gate).
3. **Encoder ROI from DXGI dirty/move rects**: the pixel-exact row-pair diff
   already elides static frames and limits color conversion; the remaining
   step is feeding rectangle hints into encoder rate control (needs runtime
   access to the MF/NVENC encoders to tune — OpenH264's ROI support is too
   limited to matter).

## P1 — transports & remaining security

4. **QUIC transport** (quinn) with the same envelopes; datagram mode for
   video, streams for control; WebTransport for the web viewer where
   available; WS stays as fallback. *Assessed 2026-07: on wired/strong-Wi-Fi
   LAN (sub-ms RTT, ~zero loss) TCP_NODELAY + latest-only send slots already
   avoid the queueing QUIC would remove, so it does not materially cut
   latency there; the win is head-of-line-blocking removal on lossy Wi-Fi.
   Browser WebTransport additionally requires TLS certs
   (serverCertificateHashes = Chromium-only today), so the web path stays WS
   regardless.*
5. OS keystore on mobile viewers (iOS Keychain / Android Keystore) — the
   host + desktop viewer already use DPAPI on Windows.
6. Certificate pinning UX for the HTTPS mode (first-connect fingerprint
   confirmation in the viewer instead of the raw browser warning).

## P2 — features

7. **Multi-monitor / multi-client layout**: several virtual monitors (driver
   already parameterized by `MaxMonitorsSupported`), per-client monitor
   assignment, video-wall spanning mode.
8. Clipboard formats beyond text (images, HTML) behind the same grant.
9. File drop host→viewer direction (download from host).
10. Audio for the desktop/mobile viewers (web viewer shipped; native ones
    need an Opus decode + playback path).

## P3 — platform breadth

11. Android/iOS CI builds (Gradle + xcodebuild GitHub runners) and store
    packaging docs.
12. Linux/macOS *hosts* (wlroots screencopy / ScreenCaptureKit) — the
    protocol and viewers are already host-OS-agnostic.
13. Opt-in remote rendezvous: end-to-end-encrypted, relay-blind (relay sees
    ciphertext only), separate binary + explicit user action; never on by
    default.

## Deliberately rejected

* Cloud accounts, telemetry, auto-update phone-home — against the product's
  core promise.
* Unsigned-driver install "hacks" — mirror mode is the honest fallback.
* Kernel-driver gamepad injection (ViGEm-style) — out of clean-room scope.
