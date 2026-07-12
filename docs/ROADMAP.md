# Roadmap

Everything below is **designed but not yet implemented** unless marked
otherwise. Ordering = impact / effort.

Recently shipped (v0.3 latency overhaul — no longer roadmap items):
independent input/video session pipelines, event-driven encode, runtime
encoder bitrate/fps (no re-init), adaptation hysteresis with sticky FPS,
single-pass BGRA→I420, TCP_NODELAY, DXGI cursor compositing, multi-monitor
input mapping, letterbox-correct touch coordinates on all viewers,
device-rate pointer sampling, latest-frame rendering everywhere.

Shipped in v0.4 (pipeline overhaul): zero-rebuild runtime bitrate raises,
parallel multi-slice encode, dirty-region encoding, dedicated low-latency
cursor channel with client overlay + composited fallback, per-stage latency
instrumentation, zero-copy in-place envelope seal, immediate-paint web
presentation, MSE/fMP4 H.264 for insecure-context browsers, hardware H.264
via Media Foundation (NVENC/QuickSync/AMF), DXGI cursor-only readback skip,
profile-switch re-baselining, QueryDisplayConfig extend-mode input mapping,
multi-monitor & multi-GPU & HDR-capable IddCx driver with CI syntax gate,
reproducible benchmark harness (`viewer/web/tests/bench.mjs`).

Shipped in v0.5 (features & security wave — no longer roadmap items):

* **SPAKE2 PAKE pairing** (RFC 9382 / P-256) across host, Rust SDK and web
  viewer (both crypto backends, byte-compat proven in CI); legacy pairing
  kept for older mobile viewers behind `allow_legacy_pair` (can be turned
  off for PAKE-only operation). Closes the offline-PIN-grinding caveat.
* **Audio**: WASAPI loopback → Opus on encrypted channel 3; off by default,
  per-session opt-in, per-client 🔊 indicators in the panel, WebCodecs
  playback with jitter cushion in the web viewer, decodable-Opus e2e tests.
* **Clipboard sync** (text) with per-device grant (deny by default, live
  panel toggle, revocation-safe), 256 KiB caps both ways, Windows
  CF_UNICODETEXT bridge with echo suppression, web viewer send/copy UX.
* **Layout-aware keyboard**: `key` alongside `code`; host injects scancodes
  for shortcuts and Unicode when the host layout differs (`VkKeyScanW`
  check) or the code is unmapped.
* **True stylus**: `CreateSyntheticPointerDevice(PT_PEN)` +
  `InjectSyntheticPointerInput` with pressure/tilt; automatic mouse
  fallback pre-1809.
* **Optional HTTPS** (`https = true` / `--https`): persistent self-signed
  cert, fingerprint printed/pinnable; SDK + desktop viewer certificate
  pinning (`--tls-fingerprint`).
* **Keystore**: desktop-viewer trust tokens DPAPI-sealed at rest on Windows.

## P0 — the driver & Windows-hardware validation

1. **Driver bring-up** (needs a WDK machine): compile `host/windows-driver`,
   test-sign, validate extend mode end-to-end, measure ring throughput at
   4K, add driver health reporting into the panel ("extend/mirror/pattern"
   badge exists server-side already).
2. **Windows-hardware runtime validation pass** for the v0.5 Windows-only
   code paths (all compile-verified by the Windows CI job, none run on real
   hardware yet — this sandbox has none; see docs/TESTING.md release gate):
   WASAPI loopback capture (incl. 44.1 kHz resample path and device-change
   recovery), clipboard bridge, synthetic-pen injection, layout-mismatch
   keyboard fallback, DPAPI store migration, MF hardware encoders (from
   v0.4).
3. **HEVC output type** for the MF encoder path (trivial variant of the same
   MFT plumbing) once decoder support is negotiated.
4. **Encoder ROI from DXGI dirty/move rects**: the pixel-exact row-pair diff
   already elides static frames and limits color conversion; the remaining
   step is feeding rectangle hints into encoder rate control (needs the
   MF/NVENC encoders — OpenH264's ROI support is too limited to matter).

## P1 — transports & remaining security niceties

5. **QUIC transport** (quinn) with the same envelopes; datagram mode for
   video, streams for control; WebTransport for the web viewer where
   available; WS stays as fallback. *Assessed 2026-07: on wired/strong-Wi-Fi
   LAN (sub-ms RTT, ~zero loss) TCP_NODELAY + latest-only send slots already
   avoid the queueing QUIC would remove, so it does not materially cut
   latency there; the win is head-of-line-blocking removal on lossy Wi-Fi.
   Browser WebTransport additionally requires TLS certs
   (serverCertificateHashes = Chromium-only today), so the web path stays WS
   regardless.*
6. **SPAKE2 for the mobile viewers** (Kotlin/Swift): the server accepts both
   methods, so Android/iOS still pair via the legacy path; porting the ~150
   lines of `pake.ts` needs per-platform EC group math (BouncyCastle /
   CryptoKit) and real devices to verify.
7. OS keystore for the remaining clients: Android Keystore hardening review,
   iOS Keychain (both apps already isolate credentials), libsecret/Keychain
   for desktop-viewer builds on Linux/macOS (Windows DPAPI shipped in v0.5).
8. Audio playback in the native desktop viewer (decode + output via cpal);
   the protocol side ships in v0.5, web playback exists.

## P2 — features

9. **File drop** with explicit accept dialog per transfer, chunked over the
   control channel with per-file size caps and progress (protocol slot:
   control messages; same grant model as clipboard).
10. **Multi-monitor / multi-client layout**: several virtual monitors (driver
    already parameterized by `MaxMonitorsSupported`), per-client monitor
    assignment, video-wall spanning mode.
11. **Gamepad forwarding** (Gamepad API → ViGEm-style injection is out of
    clean-room scope; use Windows.Gaming.Input injection when available).
12. Clipboard beyond text (images; HTML with sanitization) — the `mime`
    field is already forward-extensible.
13. Touch: `InjectTouchInput` multi-touch injection (single-touch currently
    maps to mouse; pen already uses synthetic pointers).

## P3 — platform breadth

14. Android/iOS CI builds (Gradle + xcodebuild GitHub runners) and store
    packaging docs.
15. Linux/macOS *hosts* (wlroots screencopy / ScreenCaptureKit) — the
    protocol and viewers are already host-OS-agnostic; clipboard/audio
    capture backends are trait-isolated for the same reason.
16. Opt-in remote rendezvous: end-to-end-encrypted, relay-blind (relay sees
    ciphertext only), separate binary + explicit user action; never on by
    default.

## Deliberately rejected

* Cloud accounts, telemetry, auto-update phone-home — against the product's
  core promise.
* Unsigned-driver install "hacks" — mirror mode is the honest fallback.
* Bundled JS Opus decoder for non-WebCodecs browsers (~100 KB WASM for a
  secondary feature; the audio button degrades with an explanatory tooltip).
