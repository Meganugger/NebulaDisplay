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

**Application-layer encryption, transport-agnostic.** Instead of relying on
TLS (self-signed certs on LAN = warning fatigue = users clicking through),
NDSP runs ECDH-derived AES-256-GCM *inside* the WebSocket. The same envelope
format will ride QUIC/WebTransport later without protocol changes.

**Adaptation uses measured signals only.** Send-queue backpressure (TCP
pushback measured per frame), RTT trend vs. observed minimum (bufferbloat),
and client decode-queue depth. AIMD: ×0.7 on congestion, small additive
recovery, FPS sheds only after the bitrate floor.

**Latency is measured, not guessed.** Frames carry host-clock capture
timestamps; viewers run NTP-style Ping/Pong sync and report true
capture→present latency back, visible in the overlay and the panel.

**The driver knows nothing about the network.** It only fills a triple-buffered
seqlock ring in shared memory. nebulad consumes it like any other capture
source. Driver crashes degrade to mirror mode; service crashes never take the
desktop down.

## Data flows

### Video
capture thread → `watch` channel → per-session pacing loop → encoder
(block_in_place) → `VideoFrame` header (+timestamp) → GCM envelope → WS.
Viewer: decrypt → decoder (WebCodecs/MediaCodec/VideoToolbox/OpenH264) →
present → stats back to host every second.

### Input
Viewer captures pointer/touch/pen/key → normalized events → batched per
animation frame → encrypted control channel → session checks the device's
grant (deny by default, toggled live from the panel) → `SendInput`.

### Control/health
1 Hz Ping/Pong (clock sync + RTT), 2 s HostStats push, 30 s dead-peer timeout,
single-use PINs with per-IP lockout, revocation kicks live sessions.

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
