# Testing

## Automated (run in CI on every push)

| Layer | What is actually verified |
|---|---|
| `ndsp-protocol` unit tests (28) | message serde + stable tags + forward-compat (incl. pre-v0.5 `key`-less input events), ECDH key agreement, **SPAKE2** (M/N stability, agreement, wrong-PIN divergence, nonce binding, invalid/degenerate shares, per-exchange freshness, constant-time MAC compare), wrong-PIN key mismatch, AEAD tamper rejection, envelope replay/direction/AAD attacks, video **and audio** framing, beacon parsing |
| `nebulad` unit tests (40) | JPEG/H264 encoders produce valid decodable bitstreams (SOI/Annex-B, keyframes), **runtime bitrate/fps changes without stream reset**, resolution change rebuild+IDR, BGRA→I420 byte-exact vs reference, adaptive controller (backlog cut, decrease cooldown/hysteresis, sticky fps + floor-only fps drop, sustained-RTT bufferbloat, recovery, fps restore), PIN single-use/rotation/lockout, trust store enroll/verify/revoke/persistence/transcript-MITM + **pre-v0.5 store migration with safe grant defaults**, audio resampler/downmix/test-tone, clipboard echo-suppression + baseline-no-leak + size caps, file-name sanitization + non-clobbering destinations + offer expiry/routing, TLS cert persistence with stable fingerprint, keystore roundtrip (real DPAPI on the Windows job), test-pattern motion, size parsing |
| Rust e2e (13, real sockets) | full **SPAKE2** pairing → H.264 streaming (seq order, changing payloads, keyframe first), encrypted ping/pong, wrong PIN rejected + PIN rotation (both schemes), **legacy pairing accepted by default and refused under `allow_legacy_pairing=false`**, token reconnect, input-grant live flow, revocation kick + token invalidation, client-side fingerprint pinning, **audio**: off-by-default, Opus format contract, panel mute stops the stream + listener count, PCM variant, **clipboard**: deny-by-default, grant notify, both directions, **file drop**: panel deny leaves no bytes, accept delivers bit-exact with sanitized name, corrupted transfer rejected + cleaned up, **video keeps full rate under a 240 Hz input flood** (pipeline-independence regression guard) |
| Mapping unit tests (Node) | letterbox/pillarbox coordinate math (`mapToContent`): content-box normalization, black-bar clamping, offsets, aspect mismatches |
| Node compat test (×2) | the **actual web-viewer session code** (esbuild-bundled) against the real host: **SPAKE2 pair** (proves the TS implementation is byte-compatible with the Rust one), decrypt frames, JPEG magic, encrypted ping/pong, **audio channel opt-in/format/off-switch**, token reconnect, wrong PIN — run on **both** crypto backends (native WebCrypto, and `NDSP_CRYPTO=fallback` pure-JS as in insecure browser contexts) |
| Browser E2E (Chromium) | UI pairing (SPAKE2), streaming 1280×720 with changing canvas pixels (H.264 or JPEG matching the browser's *probed* decode capability), stats overlay showing *measured* per-stage latency, input grant flow reaching the host input sink, panel PIN/QR/client list, profile switch, **cursor channel** (shape delivery + live overlay movement), **audio** (real Opus decode via WebCodecs + panel listening indicator on/off), **clipboard** (deny-by-default → grant → viewer→host sync), **file drop** (offer visible in panel, nothing written pre-accept, bit-exact delivery) |
| Compat E2E (Chromium, 6 envs) | pairing + full handshake + moving video on: secure localhost, **insecure LAN origin** (no `crypto.subtle`/`randomUUID`/WebCodecs — the real Windows/iOS/Android deployment), iOS-Safari-like (no PointerEvent/createImageBitmap/BigInt DataView/fullscreen, touch), Android-Chrome-like (touch), storage-blocked WebView, and a regression guard for the `crypto.randomUUID` crash — see `docs/BROWSER-COMPAT.md` |
| Reconnect E2E (Chromium) | host SIGKILLed mid-stream and restarted: the viewer auto-recovers by itself via token reconnect and video provably resumes (canvas pixels change) |
| Windows CI job | compiles + clippy-gates all `cfg(windows)` code (DXGI incl. cursor compositing + cursor-only readback skip, IddCx multi-ring consumer, QueryDisplayConfig input mapping, SendInput multi-monitor + layout-aware keys, **Windows Ink synthetic pen**, **WASAPI loopback**, **DPAPI keystore — roundtrip test executes for real there**, tray) |
| Driver syntax check | `host/windows-driver/tests/syntax-check.sh`: full clang syntax/type check of the IddCx driver against stub WDK headers modeled from public docs, under **both** the IddCx 1.10 and 1.4 header models |

## Benchmarks (reproducible)

`node viewer/web/tests/bench.mjs [--quick] [--json out.json]` runs the real
host + real Chromium across a resolution × profile matrix and prints a
markdown table of *measured* per-stage numbers (fps, e2e, arrival, present
wait, encode, convert, capture age, seal+send, decode, bitrate). See
`docs/BENCHMARKS.md` for the latest recorded run and how to interpret it.

Run locally: see `docs/BUILDING.md`.

## Manual test matrix (release gate)

Automated coverage cannot exercise real GPUs/networks/drivers. Before a
release, walk this matrix on real hardware and record results in the release
notes:

### Hosts
- [ ] Windows 11 (Intel iGPU) — mirror mode
- [ ] Windows 11 (NVIDIA/AMD dGPU) — mirror mode
- [ ] Windows 10 22H2 — mirror mode
- [ ] Windows 11 + test-signed driver — extend mode, monitor appears in
      Settings → Display, per-monitor scaling persists after reboot
- [ ] Sleep/wake with client connected (session resumes or reconnects cleanly)
- [ ] Driver crash simulation (kill UMDF host) → nebulad falls back to mirror

### Links
- [ ] Ethernet 1 Gbps
- [ ] Wi-Fi 5 GHz same-AP
- [ ] Wi-Fi hotspot (PC or phone as AP)
- [ ] Android USB: `adb reverse tcp:41800 tcp:41800` → 127.0.0.1 connect
- [ ] iOS wired: personal-hotspot-over-USB flow
- [ ] Degraded network: 5% loss / +100 ms (use `tc netem` or clumsy) —
      stream survives, bitrate drops, no runaway latency

### Modes / formats
- [ ] 720p / 1080p / 1440p / 4K capture (`--capture-size` and real monitors)
- [ ] 30 / 60 fps profiles; 90/120 Hz where client display supports it
- [ ] JPEG-only build (`--no-default-features`)
- [ ] H.264 on: Chrome, Edge, Safari 16.4+, Firefox (JPEG fallback where
      WebCodecs H.264 is unavailable), Android MediaCodec, iOS VideoToolbox,
      desktop viewer OpenH264

### Features (v0.5)
- [ ] WASAPI loopback audio on real hardware: music plays on the viewer,
      mute-from-panel < 1 s, indicator correct, device released at zero
      listeners (check with `pactl`-equivalent / Sound settings)
- [ ] Clipboard: Windows host ↔ web viewer both directions after grant;
      nothing syncs before the grant; 256 KiB cap enforced
- [ ] File drop from Android Chrome + desktop Chrome; deny leaves no file;
      accept lands in `<data>/received`
- [ ] `--https`: browser warning shows the panel's fingerprint; WebCodecs
      active on a LAN address (H.264 + Opus)
- [ ] DPAPI: `devices.json` starts with `NDPP1`; copy to another user
      account fails to load (fresh store), original still works

### Input / security
- [ ] input denied by default on every fresh pairing
- [ ] grant/revoke while connected takes effect < 1 s
- [ ] wrong PIN ×5 locks the IP; PIN visibly rotates in the panel
- [ ] revoked device cannot reconnect
- [ ] second host on same IP:port is refused by paired clients (fingerprint)
- [ ] touch, multi-touch scroll, keyboard incl. modifiers, wheel +
      horizontal wheel
- [ ] multi-touch injection: two-finger pinch-zoom in Maps/Photos and a
      two-finger scroll register as real touch gestures; on systems
      without synthetic pointers the first finger falls back to mouse
      press-drag and a second finger is ignored (no cursor fighting)
- [ ] stylus in Drawing mode on an ink app (e.g. Whiteboard): pressure
      varies stroke width, tilt registers, hover shows a cursor; on
      pre-1809 Windows the pen falls back to mouse strokes
- [ ] AZERTY/QWERTZ viewer typing into a US-layout host produces the
      viewer's characters; Ctrl/Alt shortcuts still act by key position

### Soak
- [ ] 4-hour continuous stream: no memory growth in nebulad (watch working
      set), no fps decay, reconnect counter stable
