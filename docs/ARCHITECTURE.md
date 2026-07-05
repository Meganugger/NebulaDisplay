# NebulaDisplay architecture

## Design goals

1. **Local-first & private** — everything works with zero internet; no
   accounts, no telemetry, encrypted transport on by default.
2. **Honest fallbacks** — every capability degrades gracefully: no driver →
   mirror mode; no hardware encoder → software JPEG; no TLS build → loopback
   testing mode. The diagnostics panel always tells the truth about which
   path is active.
3. **Low latency over throughput** — bounded queues everywhere; the system
   prefers dropping a frame to queueing it.
4. **One protocol, many viewers** — NDSP v1 is deliberately simple enough to
   implement in an afternoon on any platform (JSON control + one binary
   packet layout), which is how the web/Android/iOS viewers stay in
   lockstep.

## Component map

```
crates/nebula-proto      protocol types + framing (shared, no I/O)
crates/nebula-host       the host service
  ├─ server.rs           axum HTTPS/WS server, NDSP state machine, admin API
  ├─ pipeline.rs         per-session capture→detect→encode→send loop
  ├─ capture/            FrameSource impls: test pattern, DXGI, IddCx shm
  ├─ encode/             DirtyDetector (tile hashing) + JPEG region encoder
  ├─ adaptive.rs         AIMD quality/FPS controller + profiles
  ├─ input/              InputEvent → SendInput (Windows) / logging (dev)
  ├─ audio.rs            WASAPI loopback → PCM packets (Windows)
  ├─ pairing.rs          PIN manager + trust store (hashed tokens)
  ├─ discovery.rs        UDP beacon responder
  ├─ tls.rs              persistent self-signed cert + fingerprint
  └─ service_win.rs      Windows SCM service wrapper
host/windows-driver      IddCx UMDF2 driver → shared-memory frame export
viewer/web               browser viewer + control panel (the reference UI)
viewer/{android,ios}     native NDSP clients
viewer/desktop           Tauri shell hosting the web viewer
```

## The frame pipeline (host)

Each streaming client gets its own pipeline thread and bounded packet
channel (depth 3):

```
FrameSource.next_frame()          blocking, paced to the adaptive FPS target
  └─ DirtyDetector.detect()       64×64 tile FNV-1a hashes → changed rect
       └─ (skip encode entirely when nothing changed)
  └─ JpegRegionEncoder            BGRA→RGB repack + SIMD JPEG of the rect
  └─ VideoPacket::encode()        28-byte header + payload
  └─ mpsc::try_send               FULL? drop frame + AIMD backoff + refresh flag
```

Key decisions:

* **Bounded channel as congestion signal.** A slow socket fills the 3-slot
  channel; `try_send` failure is the earliest, cheapest congestion signal
  and caps in-flight latency at ~3 frame intervals by construction.
* **Dirty-rect + periodic full refresh.** Bounding-rect updates keep the
  viewer trivial (one `drawImage`); a forced full frame every 4s self-heals
  any missed update (and serves late-joining decoders after reconnect).
* **Why MJPEG first.** Zero inter-frame state means loss/reconnect recovery
  is free, every platform decodes it natively and fast
  (`createImageBitmap`, `BitmapFactory`, CoreGraphics), and the pure-Rust
  SIMD encoder keeps the host dependency-light. With dirty rects it is
  competitive for productivity content. The packet header already carries
  codec ids for H.264/HEVC/AV1.

### Encoder roadmap (H.264 & hardware)

The `RegionEncoder` trait is the seam: a Windows Media Foundation
implementation (`IMFTransform`, hardware MFTs: NVENC/QuickSync/AMF are
exposed uniformly through MF) plugs in without touching the pipeline. The
viewer's WebCodecs path (`VideoDecoder` with `avc1.*`) is the matching seam
client-side. Full-frame codecs disable dirty-rect mode via the existing
`full_frame` flag — no protocol change required.

## The adaptive controller

Three independent congestion signals feed one AIMD loop
(multiplicative-decrease 0.75, additive recovery every 800ms, at most one
decrease per 250ms window):

| Signal | Meaning |
|---|---|
| host `try_send` failure | the socket/network can't drain our bitrate |
| client feedback (`dropped_frames`, `queue_depth`, `decode_ms`) | the *device* can't keep up (weak tablet) — FPS is capped to `0.8 × 1000/decode_ms` |
| RTT inflation (> 3× baseline + 40ms) | bufferbloat building up along the path |

Profiles (Office/Video/Drawing/Gaming/Balanced) only set the bounds and
starting point; the controller moves freely inside them.

## The virtual display driver

`host/windows-driver` is a UMDF2 IddCx driver: Windows composes a real
desktop for each virtual monitor and hands buffers to the driver's
swap-chain thread, which copies them into a named shared-memory section
(`Global\NebulaDisplay.Frame.N` + auto-reset event). The host service's
`VirtualMonitorSource` consumes that section. Rationale:

* **User mode** — a driver crash can't BSOD; the reflector restarts it.
* **Shared memory over IOCTLs** — tiny auditable surface; torn frames are
  detected with a seq-before/seq-after check instead of cross-process locks.
* **Service/driver decoupling** — either side can restart independently;
  the service probes the section to report driver health.

## Sessions, resilience, lifecycle

* **Reconnects**: viewers auto-reconnect with exponential backoff and
  re-auth with their stored token; `Resume` forces a full-frame refresh.
  Host restarts drop nothing persistent — trust store and config are files.
* **Sleep/wake & mode changes**: DXGI duplication returns `ACCESS_LOST` on
  such transitions; the source transparently re-creates itself. The IddCx
  path re-arrives monitors via PnP.
* **Multi-client**: every client has an independent pipeline, adaptive
  state, and stats; `max_clients` guards the host. A "video wall" is just
  several clients in extend mode.
* **Windows service**: `nebula-host --service` runs under the SCM
  (installed by the Inno installer), tray app controls it.

## Web UI

One Vite MPA serves two pages from the host itself: `/` (control panel,
admin API is loopback-only) and `/view/` (the viewer). Serving the viewer
from the host means the TLS certificate the browser accepts for the page is
the same one securing the WebSocket — one prompt, everything encrypted.

## What deliberately does not exist

* Cloud relay / internet mode — out of scope until it can be done opt-in
  with end-to-end encryption (design notes in SECURITY.md).
* Unauthenticated "quick connect" — discovery never grants access.
* Telemetry of any kind.
