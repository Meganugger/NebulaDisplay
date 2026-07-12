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

Shipped since (v0.5 security & features wave — no longer roadmap items):

* **PAKE pairing (was P1.4)** — NDSP-PAKE v1, a CPace-style balanced PAKE on
  P-256 (RFC 9380 hash-to-curve): removes the offline-PIN-grinding caveat.
  Implemented in the Rust host + protocol crate, client SDK, desktop viewer
  and web viewer (both crypto backends), negotiated via `hello_ack.pake`
  with transparent legacy fallback (`allow_legacy_pairing = false` refuses
  it). Cross-stack byte compatibility pinned by RFC 9380 vectors on both
  stacks + full-handshake CI tests.
* **Optional HTTPS/WSS (was P1.7)** — `tls = true` / `--tls`: per-install
  self-signed cert, fingerprint printed at startup + shown in the panel,
  pinned-fingerprint verification in the client SDK (`Transport::TlsPinned`)
  and desktop viewer (`--tls-pin`); protects web-viewer *code* integrity on
  hostile LANs. E2E-tested (pin match streams, pin mismatch refused).
* **OS keystore at rest (was P1.6, Windows part)** — host trust store and
  desktop-viewer credentials are DPAPI-protected on Windows with transparent
  migration from plaintext stores; 0600 files elsewhere (macOS
  Keychain/secret-service still open, below).
* **Clipboard sync (was P2.9)** — text clipboard both directions with
  explicit per-device permission (panel toggle, deny by default,
  live-revocable `clipboard_grant`), 256 KiB per-event cap (refuse, never
  truncate), change-only forwarding (nothing pushed on connect), echo
  suppression, Win32 clipboard backend on Windows + in-memory backend that
  the Linux CI drives end-to-end. Web viewer UI ships send/receive buttons
  with insecure-context fallbacks.
* **Layout-aware keyboard mapping (was P2.13)** — viewers send both
  `KeyboardEvent.code` and the layout-resolved `key`; the host prefers the
  character via `VkKeyScanW` against its own layout (Unicode injection for
  AltGr-only chars), so typing and shortcuts survive layout mismatches.
* **Encoder ROI hints (was P0.3)** — dirty-rect bounding rectangles feed
  `MFSampleExtension_ROIRectangle` per-sample hints (negative QP delta) into
  Media Foundation hardware encoders that report
  `CODECAPI_AVEncVideoROIEnabled`.

## P0 — performance & the driver

1. **Driver bring-up** (needs a WDK machine): compile
   `host/windows-driver`, test-sign, validate extend mode end-to-end, measure
   ring throughput at 4K, add driver health reporting into the panel
   ("extend/mirror/pattern" badge exists server-side already).
2. **Hardware encoders — H.264 SHIPPED** (`encode/mf_h264.rs`): MFTEnumEx
   hardware enumeration (NVENC/QuickSync/AMF), async-MFT event loop at queue
   depth ≤1, MF_LOW_LATENCY + CBR + zero B-frames, runtime ICodecAPI bitrate,
   NV12 dirty-row conversion, static-frame elision, ROI dirty-rect hints,
   automatic software fallback. Compile-verified by the Windows CI job;
   **runtime validation needs a real Windows GPU machine** (this sandbox has
   none — see docs/TESTING.md release gate). Remaining: HEVC output type
   (trivial variant of the same MFT plumbing) once decoder support is
   negotiated.

## P1 — security & transports

3. **QUIC transport** (quinn) with the same envelopes; datagram mode for
   video, streams for control; WebTransport for the web viewer where
   available; WS stays as fallback. *Assessed 2026-07: on wired/strong-Wi-Fi
   LAN (sub-ms RTT, ~zero loss) TCP_NODELAY + latest-only send slots already
   avoid the queueing QUIC would remove, so it does not materially cut
   latency there; the win is head-of-line-blocking removal on lossy Wi-Fi.
   Kept at P1: real, but smaller than driver bring-up. Browser WebTransport
   additionally requires TLS certs (serverCertificateHashes = Chromium-only
   today), so the web path stays WS regardless.*
4. PAKE pairing for the **Android/iOS viewers** (the host, SDK, desktop and
   web viewers already ship it); once they do, flip the default of
   `allow_legacy_pairing` to `false` after a deprecation window.
5. macOS Keychain / Linux secret-service backends for at-rest credential
   protection (DPAPI on Windows is done).

## P2 — features

6. **Audio**: WASAPI loopback → Opus (channel 3 is reserved); per-client
   mute/volume; off by default with a visible indicator.
7. Clipboard: image/file formats behind the same grant (text shipped);
   **file drop** with explicit accept dialog per transfer.
8. **Multi-monitor / multi-client layout**: several virtual monitors (driver
   already parameterized by `MaxMonitorsSupported`), per-client monitor
   assignment, video-wall spanning mode.
9. **Gamepad forwarding** (Gamepad API → ViGEm-style injection is out of
   clean-room scope; use Windows.Gaming.Input injection when available).
10. Stylus: Windows Ink `InjectSyntheticPointerInput` for true pressure/tilt
    (current fallback maps pen to mouse).

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
