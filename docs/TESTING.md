# Testing

## Automated (run in CI on every push)

| Layer | What is actually verified |
|---|---|
| `ndsp-protocol` unit tests (23) | message serde + stable tags + forward-compat (incl. v0.4 optional-field compat), ECDH key agreement, **SPAKE2 PAKE** (agreement, wrong-PIN divergence, share blinding/freshness, bad-point rejection, pinned KDF vector), wrong-PIN key mismatch, AEAD tamper rejection, envelope replay/direction/AAD attacks, media framing, beacon parsing |
| `nebulad` unit tests (18) | JPEG/H264 encoders produce valid decodable bitstreams (SOI/Annex-B, keyframes), **runtime bitrate/fps changes without stream reset**, resolution change rebuild+IDR, BGRA→I420 byte-exact vs reference, adaptive controller (backlog cut, decrease cooldown/hysteresis, sticky fps + floor-only fps drop, sustained-RTT bufferbloat, recovery, fps restore), PIN single-use/rotation/lockout, trust store enroll/verify/revoke/persistence/transcript-MITM, test-pattern motion, size parsing |
| Rust e2e (8, real sockets) | full PAKE pairing → H.264 streaming (seq order, changing payloads, keyframe first), encrypted ping/pong, wrong PIN rejected + PIN rotation, **legacy PIN-HKDF pairing back-compat** (pair, token reconnect, wrong PIN), token reconnect, input-grant live flow, **clipboard grant flow** (deny-by-default, live grant notify, host application, no-echo fan-out, size-cap rejection), revocation kick + token invalidation, client-side fingerprint pinning, **video keeps full rate under a 240 Hz input flood** (pipeline-independence regression guard) |
| Mapping unit tests (Node) | letterbox/pillarbox coordinate math (`mapToContent`): content-box normalization, black-bar clamping, offsets, aspect mismatches |
| Node compat test (×2) | the **actual web-viewer session code** (esbuild-bundled) against the real host: **SPAKE2 pairing byte-compat with the Rust implementation**, decrypt frames, JPEG magic, encrypted ping/pong, token reconnect, **clipboard grant + push + cross-viewer fan-out + no-echo**, wrong PIN — run on **both** crypto backends (native WebCrypto, and `NDSP_CRYPTO=fallback` pure-JS as in insecure browser contexts) |
| Browser E2E (Chromium) | UI pairing (SPAKE2), streaming 1280×720 with changing canvas pixels (H.264 or JPEG matching the browser's *probed* decode capability), stats overlay showing *measured* per-stage latency, input grant flow reaching the host input sink, panel PIN/QR/client list, profile switch, **cursor channel** (shape delivery + live overlay movement), **clipboard sync** (grant-gated toolbar UI, send to host, second viewer receives, origin untouched) |
| Compat E2E (Chromium, 6 envs) | pairing + full handshake + moving video on: secure localhost, **insecure LAN origin** (no `crypto.subtle`/`randomUUID`/WebCodecs — the real Windows/iOS/Android deployment), iOS-Safari-like (no PointerEvent/createImageBitmap/BigInt DataView/fullscreen, touch), Android-Chrome-like (touch), storage-blocked WebView, and a regression guard for the `crypto.randomUUID` crash — see `docs/BROWSER-COMPAT.md` |
| Reconnect E2E (Chromium) | host SIGKILLed mid-stream and restarted: the viewer auto-recovers by itself via token reconnect and video provably resumes (canvas pixels change) |
| Windows CI job | compiles + clippy-gates all `cfg(windows)` code (DXGI incl. cursor compositing + cursor-only readback skip, IddCx multi-ring consumer, QueryDisplayConfig input mapping, SendInput multi-monitor mapping, layout-aware key injection, synthetic pen injection, clipboard read/write/watcher, DPAPI trust-store protection, tray) |
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

### Input / security
- [ ] input denied by default on every fresh pairing
- [ ] grant/revoke while connected takes effect < 1 s
- [ ] wrong PIN ×5 locks the IP; PIN visibly rotates in the panel
- [ ] revoked device cannot reconnect
- [ ] second host on same IP:port is refused by paired clients (fingerprint)
- [ ] touch, multi-touch scroll, stylus pressure/tilt/hover (Drawing mode —
      verify real Windows Ink pressure in e.g. Paint/Whiteboard, and the
      mouse fallback on Win10 < 1809), keyboard incl. modifiers, wheel +
      horizontal wheel
- [ ] mismatched keyboard layouts (e.g. AZERTY viewer → QWERTY host): letters,
      shifted symbols, AltGr characters, Ctrl+C/V shortcuts stay positional
- [ ] clipboard denied by default; grant syncs both directions (PC↔viewer);
      revoke stops it < 1 s; 256 KiB cap enforced; Windows watcher picks up
      host-side copies
- [ ] trust store on Windows is DPAPI-wrapped (`devices.json` starts with
      `{"dpapi":`), and a pre-0.5 plaintext store migrates on first write

### Soak
- [ ] 4-hour continuous stream: no memory growth in nebulad (watch working
      set), no fps decay, reconnect counter stable
