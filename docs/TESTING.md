# Testing

## Automated (run in CI on every push)

| Layer | What is actually verified |
|---|---|
| `ndsp-protocol` unit tests (28) | message serde + stable tags + forward-compat (incl. v1.0 pair/auth_ok/key wire compat), ECDH key agreement, wrong-PIN key mismatch, AEAD tamper rejection, envelope replay/direction/AAD attacks, video+audio media framing, beacon parsing, **SPAKE2**: agreement, wrong-PIN divergence, ECDH-transcript binding, invalid/identity share rejection, share unlinkability, fixed cross-stack vector |
| `nebulad` unit tests (29) | JPEG/H264 encoders produce valid decodable bitstreams (SOI/Annex-B, keyframes), **runtime bitrate/fps changes without stream reset**, resolution change rebuild+IDR, BGRA→I420 byte-exact vs reference, adaptive controller (backlog cut, decrease cooldown/hysteresis, sticky fps + floor-only fps drop, sustained-RTT bufferbloat, recovery, fps restore), PIN single-use/rotation/lockout, trust store enroll/verify/revoke/persistence/transcript-MITM, test-pattern motion, size parsing, layout-aware key planning (AZERTY char vs positional), clipboard backend sequencing |
| Rust e2e (9, real sockets) | full **SPAKE2** pairing → H.264 streaming (seq order, changing payloads, keyframe first), encrypted ping/pong, wrong PIN rejected + PIN rotation, token reconnect, input-grant live flow, revocation kick + token invalidation, client-side fingerprint pinning, **video keeps full rate under a 240 Hz input flood** (pipeline-independence regression guard), legacy-pairing accept/refuse per `legacy_pin_pairing`, **clipboard** deny-by-default → grant → both directions → size cap, **audio** silent-before-opt-in → Opus 48 kHz stereo with monotonic seq → silent-after-opt-out |
| Mapping unit tests (Node) | letterbox/pillarbox coordinate math (`mapToContent`): content-box normalization, black-bar clamping, offsets, aspect mismatches |
| Node compat test (×2) | the **actual web-viewer session code** (esbuild-bundled) against the real host: SPAKE2 pair, decrypt frames, JPEG magic, encrypted ping/pong, audio opt-in gating + Opus packets, clipboard deny-by-default, token reconnect, wrong PIN — run on **both** crypto backends (native WebCrypto, and `NDSP_CRYPTO=fallback` pure-JS as in insecure browser contexts) |
| PAKE cross-stack vectors (Node) | `tests/pake-vectors.mjs`: the TypeScript SPAKE2 (src/pake.ts) reproduces the Rust fixed vector byte-for-byte (shares + derived pair key), wrong-PIN divergence, randomized shares, invalid-share rejection |
| Browser E2E (Chromium) | UI pairing, streaming 1280×720 with changing canvas pixels (H.264 or JPEG matching the browser's *probed* decode capability), stats overlay showing *measured* per-stage latency, input grant flow reaching the host input sink, panel PIN/QR/client list, profile switch, **cursor channel** (shape delivery + live overlay movement), **audio** (toolbar opt-in → WebCodecs Opus decode with packets counted in the overlay → panel privacy indicator naming the listener → opt-out clears it), **clipboard** (panel grant API + sync toggle + push path) |
| Compat E2E (Chromium, 6 envs) | pairing + full handshake + moving video on: secure localhost, **insecure LAN origin** (no `crypto.subtle`/`randomUUID`/WebCodecs — the real Windows/iOS/Android deployment), iOS-Safari-like (no PointerEvent/createImageBitmap/BigInt DataView/fullscreen, touch), Android-Chrome-like (touch), storage-blocked WebView, and a regression guard for the `crypto.randomUUID` crash — see `docs/BROWSER-COMPAT.md` |
| Reconnect E2E (Chromium) | host SIGKILLed mid-stream and restarted: the viewer auto-recovers by itself via token reconnect and video provably resumes (canvas pixels change) |
| Windows CI job | compiles + clippy-gates all `cfg(windows)` code (DXGI incl. cursor compositing + cursor-only readback skip, IddCx multi-ring consumer, QueryDisplayConfig input mapping, SendInput multi-monitor mapping incl. VkKeyScanW layout-aware keys, WASAPI loopback capture, CF_UNICODETEXT clipboard, DPAPI trust-store wrap, tray) |
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
- [ ] touch, multi-touch scroll, stylus pressure (Drawing mode), keyboard incl.
      modifiers, wheel + horizontal wheel
- [ ] non-QWERTY viewer layout (e.g. AZERTY) types the characters shown on
      its own keycaps into the host (layout-aware `key` path)
- [ ] clipboard: denied by default; grant → text syncs both ways; >256 KiB
      refused; revoking the grant stops sync immediately
- [ ] audio (real WASAPI): off by default; viewer opt-in starts playback of
      what the host plays; panel indicator lists the listener; per-client
      mute silences without disconnecting; device default-output switch
      mid-stream recovers
- [ ] DPAPI trust store: pre-v0.5 plaintext devices.json migrates on first
      write; store is unreadable from another Windows account

### Soak
- [ ] 4-hour continuous stream: no memory growth in nebulad (watch working
      set), no fps decay, reconnect counter stable
