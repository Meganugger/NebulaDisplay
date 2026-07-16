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

Shipped in v0.5 (no longer roadmap items):

* **PAKE pairing (was P1.4)** — SPAKE2 over P-256 with mutual confirmation
  MACs (`shared/protocol/src/spake2.rs`), spoken by the web viewer (both
  crypto backends, verified byte-compatible cross-stack in CI) and the
  desktop viewer / client SDK. Legacy PIN-HKDF pairing remains for the
  mobile apps and is host-disableable (`allow_legacy_pairing = false`).
* **Audio (was P2.8)** — WASAPI loopback (Windows) / test tone (elsewhere)
  → Opus 48 kHz stereo on channel 3, with a raw-PCM variant for web viewers
  on insecure origins (no WebCodecs there). Per-viewer opt-in, per-device
  panel mute, live "listening" indicator, capture device released at zero
  listeners. Chromium E2E decodes real Opus in CI; WASAPI runtime needs a
  Windows machine (same gate as DXGI — docs/TESTING.md).
* **Clipboard sync (was P2.9)** — text, both directions, deny-by-default
  per-device grant, 256 KiB cap, echo suppression, never ships pre-session
  clipboard content. `arboard` backend with headless in-memory fallback.
* **File drop (was P2.10)** — viewer→host with an explicit per-transfer
  accept in the panel, sanitized filenames, size caps, in-order chunking,
  sha256 verification, automatic cleanup of failed/denied transfers.
* **Layout-aware keyboard (was P2.13)** — viewers send `code` + `key`; the
  Windows sink injects the exact character (KEYEVENTF_UNICODE) when the
  host layout would render a different glyph, and keeps scancode semantics
  for shortcuts/named keys.
* **Optional HTTPS (was P1.7)** — `--https` with a persistent self-signed
  cert; fingerprint pinned in panel + banner; unlocks secure-context
  browser features (WebCodecs H.264/Opus, clipboard API) on LAN addresses.
* **Stylus injection (was P2.14)** — Windows Ink synthetic pen
  (`CreateSyntheticPointerDevice`/`InjectSyntheticPointerInput`): remote pen
  strokes carry real pressure + tilt into ink-aware apps, with hover
  support and automatic mouse fallback where the API is unavailable.
* **At-rest key protection (was P1.6, Windows part)** — DPAPI wrapping of
  the trust store, identity key, and TLS key with transparent migration
  from plaintext stores. (Android already uses the platform Keystore; a
  macOS/Linux keychain backend remains open below.)

## P1 — security & transports

4. **SPAKE2 on Android/iOS**, then flip `allow_legacy_pairing` default to
   off — removes the last offline-grinding caveat in SECURITY.md. Requires
   EC group arithmetic on the platforms (BouncyCastle / swift-crypto HPKE
   primitives or a small vetted implementation).
5. **QUIC transport** (quinn) with the same envelopes; datagram mode for
   video, streams for control; WebTransport for the web viewer where
   available; WS stays as fallback. *Assessed 2026-07: on wired/strong-Wi-Fi
   LAN (sub-ms RTT, ~zero loss) TCP_NODELAY + latest-only send slots already
   avoid the queueing QUIC would remove, so it does not materially cut
   latency there; the win is head-of-line-blocking removal on lossy Wi-Fi.
   Kept at P1: real, but smaller than hardware encoders (P0.2). Browser
   WebTransport additionally requires TLS certs (serverCertificateHashes =
   Chromium-only today), so the web path stays WS regardless.*
6. Keychain/keyring backends for trust tokens on macOS/Linux hosts (Windows
   DPAPI + Android Keystore shipped).

## P2 — features

11. **Multi-monitor / multi-client layout**: several virtual monitors (driver
    already parameterized by `MaxMonitorsSupported`), per-client monitor
    assignment, video-wall spanning mode.
12. **Gamepad forwarding** (Gamepad API → ViGEm-style injection is out of
    clean-room scope; use Windows.Gaming.Input injection when available).
14. Touch: multi-touch injection via the same synthetic-pointer API
    (single-finger touch currently maps to mouse; the pen path shipped in
    v0.5 provides the plumbing pattern).
15. Host→viewer file send (viewer→host shipped in v0.5); audio for the
    desktop/mobile viewers (web shipped).

## P3 — platform breadth

16. Android/iOS CI builds (Gradle + xcodebuild GitHub runners) and store
    packaging docs.
17. Linux/macOS *hosts* (wlroots screencopy / ScreenCaptureKit) — the
    protocol and viewers are already host-OS-agnostic.
18. Opt-in remote rendezvous: end-to-end-encrypted, relay-blind (relay sees
    ciphertext only), separate binary + explicit user action; never on by
    default.

## Deliberately rejected

* Cloud accounts, telemetry, auto-update phone-home — against the product's
  core promise.
* Unsigned-driver install "hacks" — mirror mode is the honest fallback.
