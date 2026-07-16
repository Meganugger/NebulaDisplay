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

Shipped in v0.6 (no longer roadmap items):

* **HEVC hardware encode (was P0.2 remainder)** — the MF encoder
  (`encode/mf_video.rs`) now produces H.264 *or* HEVC from the same
  async-MFT plumbing; HEVC is hardware-only and only negotiated when an
  MFT exists and the viewer probes real WebCodecs HEVC decode.
* **Encoder ROI hints (was P0.3)** — dirty-row bounds ride each MF input
  sample as `MFSampleExtension_ROIRectangle` (QPΔ −4) when the encoder
  supports `CODECAPI_AVEncVideoROIEnabled`: rate control spends its budget
  where pixels changed.
* **QUIC transport (was P1.5)** — quinn endpoint on the same port (UDP,
  ALPN `ndsp/1`); control on a bidi stream, audio on an ordered uni
  stream, one uni stream per video frame (no cross-frame head-of-line
  blocking on lossy Wi-Fi; stale frames dropped by the envelope
  counters). Native viewers opt in with `--quic`. Full loopback E2E.
* **Keychain at rest on Linux/macOS (was P1.6 remainder)** — keystore
  files sealed with AES-256-GCM under a wrapping key in Secret Service /
  macOS Keychain; plaintext-0600 fallback on headless systems.
* **SPAKE2 on Android (was P1.4, Android half)** — BouncyCastle P-256
  implementation, byte-compatibility with the Rust reference proven by a
  25-round cross-stack exchange in CI (`viewer/android/interop/`).
* **Multi-touch injection (was P2 item 14)** — up to 10 contacts through
  synthetic pointer devices with a unit-tested full-frame tracker;
  pinch/rotate reach apps as real gestures; mouse fallback preserved.
* **Gamepad forwarding (was P2.12)** — web Gamepad API snapshots →
  `Windows.Gaming.Input` injection (UWP/GDK titles). XInput-only Win32
  games would need a bus driver — deliberately out of clean-room scope.
* **Host→viewer file send + desktop viewer audio (was P2.15)** — panel
  upload → explicit viewer accept → verified transfer (sha256), web +
  desktop receivers; desktop viewer plays host audio (Opus/cpal, F9).
* **Mobile CI gates (was P3.16, partial)** — Android `assembleDebug` and
  iOS Swift type-check jobs (advisory until proven on runners), plus the
  gating SPAKE2 interop job.

## P0 — performance & the driver

1. **Driver bring-up** (needs a WDK machine): compile
   `host/windows-driver`, test-sign, validate extend mode end-to-end, measure
   ring throughput at 4K, add driver health reporting into the panel
   ("extend/mirror/pattern" badge exists server-side already).
2. **Hardware-encoder runtime validation** (needs a real Windows GPU
   machine): the MF H.264/HEVC paths and ROI hints are compile-verified by
   the Windows CI job; docs/TESTING.md lists the release-gate checklist.

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

4. **SPAKE2 on iOS** (Android shipped in v0.6), then flip
   `allow_legacy_pairing` default to off — removes the last
   offline-grinding caveat in SECURITY.md. Needs P-256 group arithmetic in
   Swift (CryptoKit exposes none): a vetted Swift EC library or a small
   audited implementation, verified against
   `shared/protocol/examples/spake2_interop.rs` the way the Kotlin one is.
5. **WebTransport for the web viewer** — blocked on browser support
   (`serverCertificateHashes` is Chromium-only); WS stays the web path.
   Native QUIC shipped in v0.6.

## P2 — features

11. **Multi-monitor / multi-client layout**: several virtual monitors (driver
    already parameterized by `MaxMonitorsSupported`), per-client monitor
    assignment, video-wall spanning mode. (Host-wide monitor selection
    already exists via `--display-index`; per-client assignment needs a
    capture hub with per-monitor channels — a deliberate, separate change
    to the hottest path.)
15. Audio for the **mobile** viewers (web + desktop shipped); host→viewer
    file send for the mobile viewers.

## P3 — platform breadth

16. Promote the Android build / iOS type-check CI jobs from advisory to
    gating once proven on runners; store packaging docs.
17. Linux/macOS *hosts* (wlroots screencopy / ScreenCaptureKit) — the
    protocol and viewers are already host-OS-agnostic.
18. Opt-in remote rendezvous: end-to-end-encrypted, relay-blind (relay sees
    ciphertext only), separate binary + explicit user action; never on by
    default.

## Deliberately rejected

* Cloud accounts, telemetry, auto-update phone-home — against the product's
  core promise.
* Unsigned-driver install "hacks" — mirror mode is the honest fallback.
