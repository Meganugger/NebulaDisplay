# Changelog

## v0.6.0 — 2026-07-16

* **HEVC hardware encode** (Media Foundation, hardware-only) with WebCodecs
  decode probing in the web viewer; codec negotiation now spans everything
  the host can actually encode.
* **Encoder ROI hints**: dirty-region bounds steer MF rate control
  (`MFSampleExtension_ROIRectangle`, QPΔ −4).
* **QUIC transport** (quinn, UDP on the same port, ALPN `ndsp/1`):
  per-frame video streams (no cross-frame head-of-line blocking), ordered
  audio lane, control on the bidirectional stream. `nebula-viewer --quic`.
* **True multi-touch injection** (up to 10 contacts) via synthetic pointer
  devices; stuck-gesture release on disconnect.
* **Gamepad forwarding**: web Gamepad API → `Windows.Gaming.Input`
  injection (UWP/GDK titles).
* **Host→viewer file send** with explicit viewer-side accept (web dialog /
  `--receive-dir`), sha256-verified; panel gains per-client "Send file".
* **Desktop viewer audio** (Opus → cpal, F9 toggle, jitter-bounded queue).
* **OS-keychain keystore sealing on Linux/macOS** (Secret Service / macOS
  Keychain; `NDSP_NO_KEYCHAIN=1` opt-out for headless boxes).
* **SPAKE2 pairing on Android**, proven byte-compatible with the Rust
  reference by a CI interop exchange; legacy pairing now needed only by iOS.
* CI: macOS job, SPAKE2 interop job (gating); Android build + iOS
  type-check jobs (advisory).

## v0.5.0

SPAKE2 pairing (web/desktop), WASAPI→Opus audio, clipboard sync, file drop,
optional HTTPS, DPAPI keystore, layout-aware keyboard, Windows Ink stylus.

## v0.4.0

Hardware H.264 (MF), dirty-region encoding, cursor channel, per-stage
latency instrumentation, MSE/fMP4 fallback, IddCx driver completion,
benchmark harness.

## v0.3.0

Latency overhaul: independent input/video pipelines, event-driven encode,
runtime encoder reconfiguration, adaptation hysteresis.
