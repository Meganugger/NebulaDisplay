# NebulaDisplay Architecture

NebulaDisplay turns other devices into extra (or mirrored) monitors for a
Windows PC over the local network — an original, clean-room implementation
with its own protocol (NDSP), no cloud, no accounts, no telemetry.

## Component map

```
┌────────────────────────── Windows host ──────────────────────────┐
│                                                                   │
│  DWM ──► IddCx virtual monitor ──► frame ring ──┐                 │
│          (host/windows-driver, extend mode)     │                 │
│                                                 ▼                 │
│  Desktop ──► DXGI duplication ────────────► nebulad (host/service)│
│              (mirror mode, no driver)        │  capture           │
│                                              │  encode (H264/JPEG)│
│  SendInput ◄── input bridge ◄────────────────┤  AES-256-GCM       │
│                                              │  adapt (AIMD)      │
│  nebula-tray (host/tray-ui) ──► loopback ────┤  discovery (UDP)   │
│  panel.html  (127.0.0.1 only) ──────────────►│  panel API         │
└──────────────────────────────────────────────┼───────────────────┘
                                               │ NDSP over WebSocket
              ┌────────────────┬───────────────┼────────────┬──────────────┐
              ▼                ▼               ▼            ▼              ▼
        web viewer      desktop viewer     Android        iOS         (future)
        (viewer/web)    (viewer/desktop)  (viewer/       (viewer/
        WebCodecs/JPEG  winit+softbuffer   android)       ios)
                                          MediaCodec    VideoToolbox
```

## Crate/module layout

| Path | Language | Role |
|---|---|---|
| `shared/protocol` | Rust | NDSP v1: control messages, encrypted envelopes, media framing, handshake crypto, discovery beacons. The single wire-format authority. |
| `shared/client` | Rust | Client SDK (pair / token reconnect / encrypted session) used by the desktop viewer and by integration tests against the real server. |
| `host/service` (`nebulad`) | Rust | The host: capture sources, encoders, per-client sessions, adaptation, PIN/trust security, discovery, loopback panel. |
| `host/windows-driver` | C++ | IddCx UMDF driver: real virtual monitor, swap-chain processing, shared-memory frame ring. |
| `host/tray-ui` | Rust | Windows notification-area companion (thin HTTP client of the panel API). |
| `viewer/web` | TypeScript | Zero-install browser viewer + host control panel (served by nebulad). |
| `viewer/desktop` | Rust | Portable native viewer (Windows/macOS/Linux). |
| `viewer/android` / `viewer/ios` | Kotlin / Swift | Native mobile viewers (same protocol, hardware decode). |

## Key design decisions

**Capture is a trait, sources are hot-swappable.** `FrameSource` has three
implementations chosen at startup in priority order: IddCx ring (extend) →
DXGI duplication (mirror) → synthetic test pattern (CI/dev). Everything
downstream is identical, which is why the whole pipeline is testable on Linux.

**One encoder per client.** Sessions own their encoder so quality adapts per
client (a phone on Wi-Fi and a wired laptop get different bitrates). Frames
are shared zero-copy (`Arc<CapturedFrame>` in a `tokio::sync::watch`); an
encoder only runs when its client is keeping up — slow clients naturally skip
to the newest frame instead of queueing stale ones.

**Input never waits behind video.** Each session is four independent tasks:
the *pump* decrypts inbound envelopes and applies input / answers pings the
instant they arrive; the *video task* encodes event-driven (a new capture
starts encoding immediately, rate-limited to the adaptive fps); the *writer*
owns the socket and lets control messages preempt video, with video flowing
through a latest-only slot (bounded everywhere, stale frames dropped, pending
keyframes protected). No stage can block another; there is no shared pacing
timer to disturb. Every queue in the pipeline is bounded at 1 (video) or
small-and-preempting (control).

**Application-layer encryption, transport-agnostic.** Instead of relying on
TLS (self-signed certs on LAN = warning fatigue = users clicking through),
NDSP runs ECDH-derived AES-256-GCM *inside* the WebSocket. The same envelope
format will ride QUIC/WebTransport later without protocol changes.

**Adaptation uses measured signals only — with hysteresis.** Send-queue
backpressure (TCP pushback measured per frame), RTT trend vs. observed
minimum (bufferbloat; pings are answered off the fast path so encode time
never pollutes RTT), and client decode-queue depth. Every signal must be
*sustained* before acting, a 1.5 s cooldown follows every decrease, and
recovery probes at ≈8 %/s. FPS is sticky: it only drops after the bitrate
floor plus continued pressure, and is restored in one step after 8 s of
clean link — so pacing stays even instead of oscillating. Bitrate and fps
changes are applied to the encoder at runtime (`SetOption`); the encoder is
only ever rebuilt on resolution change.

**Latency is measured, not guessed.** Frames carry host-clock capture
timestamps; viewers run NTP-style Ping/Pong sync and report true
capture→present latency back, visible in the overlay and the panel. Every
pipeline stage is individually instrumented — capture age at encode start,
color-convert share, encode, seal+send, capture→arrival, decode, and
decode→paint wait — so a regression in any stage is attributable, not folded
into one opaque number. `viewer/web/tests/bench.mjs` turns this into a
reproducible benchmark matrix.

**Dirty-region encoding.** Each encoder diffs every frame against its
previous one in exact row pairs (SIMD `memcmp`, no hashes — no false
"unchanged" ever). Fully static frames are elided entirely: no color
conversion, no encode, no packet (a keyframe owed to a resyncing decoder is
still served). Partially changed frames color-convert only the changed row
pairs. Result: desktop idle costs ~zero CPU/bandwidth; text editing, window
dragging and scrolling convert only the moved region; full-screen video
performs as before (diff cost ≈ 0.4–0.8 ms at 1080p, repaid by the skips).

**Cursor rides its own channel.** DXGI pointer updates (position + shape)
are forwarded as `CursorShape`/`CursorPos` control messages — never queued
behind video frames — and rendered client-side as a hotspot-correct scaled
overlay. Cursor motion over a static desktop therefore costs a few dozen
bytes instead of a GPU readback + encode + full frame (the DXGI source skips
the whole readback for cursor-only acquisitions). When a legacy viewer that
can't render the overlay connects, the host automatically falls back to
compositing the cursor into the video for everyone (`ClientInfo.features`
negotiates this).

**The driver knows nothing about the network.** It only fills a triple-buffered
seqlock ring in shared memory. nebulad consumes it like any other capture
source. Driver crashes degrade to mirror mode; service crashes never take the
desktop down.

## Data flows

### Video
capture thread (recycled buffers, idle-parked) → `watch` channel →
per-session event-driven encode task (block_in_place; row-pair dirty diff →
static-frame elision / partial single-pass BGRA→I420; multi-slice parallel
encode) → latest-only slot → writer task → in-place GCM seal of
header‖payload (`seal_parts`, no concatenation copy) → WS (TCP_NODELAY).
Viewer: decrypt → decoder (WebCodecs/MediaCodec/VideoToolbox/OpenH264) →
immediate paint on decode (desynchronized canvas; a microtask coalesces
decode bursts to the newest frame) → stats back to host every second.

### Cursor
DXGI pointer updates → `watch` channel → per-session forwarder (control
channel, preempts video in the writer) → client-side overlay positioned in
letterbox space with hotspot + scale correction. Static-desktop mouse motion
never touches the video pipeline.

### Input
Viewer captures pointer/touch/pen/key → letterbox-corrected normalized
coordinates → discrete events sent immediately, move samples coalesced ≤4 ms
(device-rate via pointerrawupdate/getCoalescedEvents where available) →
encrypted control channel → session pump applies them the moment they
decrypt (never queued behind video) after checking the device's grant
(deny by default, toggled live from the panel) → `SendInput` mapped through
the captured monitor's desktop rect (multi-monitor correct).

### Control/health
2 Hz Ping/Pong (clock sync + RTT, answered off the fast path), 2 s HostStats
push, 30 s dead-peer timeout, single-use PINs with per-IP lockout, revocation
kicks live sessions.

## Fault handling

| Failure | Behavior |
|---|---|
| Driver missing/unsigned | Automatic mirror mode; panel shows which source is active |
| DXGI access lost (UAC, mode change) | Duplication re-created, stream continues |
| Encoder error | Logged, frame skipped, session survives; decoder can request keyframes |
| Network stall | Backpressure → AIMD cut → FPS shed; jitter absorbed client-side |
| Client vanish | 30 s recv timeout reaps the session |
| Trust store corruption | Quarantined to `.bak`, host keeps running with empty store |
| Wrong PIN ×N | PIN rotates every failure; per-IP lockout after `max_pin_attempts` |
