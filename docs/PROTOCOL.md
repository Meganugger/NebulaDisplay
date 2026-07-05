# NDSP — NebulaDisplay Stream Protocol v1

NDSP is the original wire protocol between NebulaDisplay hosts and viewers.
Reference implementations: `crates/nebula-proto` (Rust) and
`viewer/web/src/protocol.ts` (TypeScript); Android/iOS clients implement the
same format.

## Transport

* v1: **WebSocket**, TLS by default (self-signed host cert, fingerprint
  pinned via QR/discovery; browsers use the standard certificate prompt).
* Planned additive transports: WebTransport/QUIC datagrams for video (same
  packet bytes), negotiated via capability strings — no version bump.
* **Text frames** carry JSON control messages; **binary frames** carry
  media packets.

## Versioning rules

* `Hello{min_version, max_version}` ↔ `HelloAck{version}`; the host picks
  the highest shared version, or answers `Error{protocol_mismatch}`.
* Receivers MUST ignore unknown JSON `type`s and unknown fields
  (tested in `nebula-proto`).
* Binary layouts are frozen; a new layout uses a new `packet_version` byte.
* Capability strings (`video/h264`, `audio/opus`, …) gate features without
  version bumps.

## Connection state machine

```
connect ──► Hello ──► HelloAck ──┬─ known device ─► Auth ─► AuthOk ─┐
                                 └─ new device ──► PairRequest ─► PairOk ─┤
                                                                          ▼
                 SessionStart ─► SessionStarted ─► [video/audio packets, Input,
                                                    Ping/Pong, Feedback, Stats]
                 SessionStop / Bye / socket loss ─► reconnect + Auth + Resume
```

Nothing streams and no input is accepted before `AuthOk`/`PairOk`.

## Control messages (JSON, tagged `type`)

| Type | Direction | Purpose |
|---|---|---|
| `hello` / `hello_ack` | C→H / H→C | version + capability negotiation; `known_device` tells the client whether token auth can work |
| `pair_request` / `pair_ok` | C→H / H→C | single-use PIN → 256-bit device token (hash stored host-side) |
| `auth` / `auth_ok` | C→H / H→C | token auth; `auth_ok.input_allowed` reports the input grant |
| `session_start` / `session_started` | C→H / H→C | mode (mirror/extend), profile, viewport, codec list, audio opt-in → chosen codec/mode |
| `session_stop` | both | stop streaming, keep connection |
| `mode_change` | C→H | switch profile / preferred mode mid-session |
| `input` | C→H | batched `InputEvent`s (below); silently dropped unless granted |
| `input_permission` | H→C | live grant/revoke notification |
| `clipboard_offer/accept/data` | both | explicit-permission clipboard sync (host master switch, off by default) |
| `ping` / `pong` | both | RTT probes (echo `t_micros`) |
| `feedback` | C→H | client decode stats for the adaptive controller |
| `stats` | H→C | host stream stats for overlays |
| `error` | H→C | typed error codes (below) |
| `bye`, `resume`, `resume_ok` | both | graceful close / fast re-attach (forces full-frame refresh) |

Error codes: `protocol_mismatch`, `bad_pin`, `pin_expired`, `bad_token`,
`not_authorized`, `input_denied`, `busy`, `internal`, `unsupported_codec`.

### Input events

Coordinates are normalized `0..1` in stream space (resolution-independent).

```jsonc
{"kind":"mouse_move","x":0.5,"y":0.25}
{"kind":"mouse_button","button":"left","down":true,"x":0.5,"y":0.25}
{"kind":"mouse_wheel","dx":0,"dy":1.0}
{"kind":"key","code":"KeyA","down":true}            // W3C KeyboardEvent.code
{"kind":"touch","id":3,"phase":"move","x":0.1,"y":0.9,"pressure":0.7}
{"kind":"stylus","x":0.3,"y":0.3,"pressure":0.5,"tilt_x":0.1,"tilt_y":null,
 "down":true,"eraser":false}
```

## Binary packets

All integers little-endian. First byte = channel.

### Video (channel `0x01`), header 28 bytes

| Offset | Size | Field |
|---|---|---|
| 0 | 1 | channel = 0x01 |
| 1 | 1 | packet_version = 1 |
| 2 | 1 | codec: 1 JPEG, 2 H.264, 3 HEVC, 4 AV1 |
| 3 | 1 | flags: bit0 full-frame, bit1 keyframe |
| 4 | 4 | frame_id (u32, wraps) |
| 8 | 8 | capture_ts (µs, host monotonic) |
| 16 | 2×4 | x, y, w, h — dirty rect in stream pixels |
| 24 | 2×2 | stream_w, stream_h — full canvas size |
| 28 | … | payload (JPEG image of the rect / codec bitstream) |

Rendering rule: draw payload at (x, y) on a persistent stream-sized canvas;
resize the canvas when `stream_w/h` change; `full_frame` covers everything.

### Audio (channel `0x02`), header 20 bytes

channel, version=1, codec (1 PCM s16le, 2 Opus), channels, seq u32,
capture_ts u64, sample_rate u32, payload.

## Discovery (UDP, untrusted)

Probe: datagram starting with `NDSP-DISCOVER-1` to port 38471 (broadcast).
Reply: `{"service":"nebuladisplay","version":1,"name":…,"port":…,"tls":…,
"tls_fingerprint":…}`.
Discovery only advertises; pairing is always required. Web viewers use
QR/manual connect instead (no UDP in browsers).

## QR pairing payload

The control panel encodes a viewer URL:
`https://<ip>:<port>/view/#host=<ip>:<port>&pin=<pin>&autoconnect=1`.
Native viewers may instead scan the JSON payload from
`POST /api/admin/pin`: `{"v":1,"kind":"nebuladisplay-pair","port":…,
"tls":…,"fp":"<sha256>","pin":"…"}` (fp enables strict pinning before the
first byte is sent).

## Backward-compatibility plan

* v1 clients ↔ v(n) hosts: hosts keep v1 support until a documented
  deprecation; `negotiate_version` picks the shared version.
* New media codecs: new capability + codec id, same packet layout.
* New transports: new capability (`transport/webtransport`), fallback to WS.
* Schema evolution is tested: unknown-field / unknown-type tests live in
  `nebula-proto` and must keep passing.
