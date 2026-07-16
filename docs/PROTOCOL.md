# NDSP — NebulaDisplay Stream Protocol v1

Status: implemented (Rust host + Rust/web/Kotlin/Swift clients).
Authority: `shared/protocol` — this document describes what that code does.

## Transport

* WebSocket (`ws://host:41800/ndsp`), binary subprotocol described below.
* Plain HTTP serves the web viewer statics on the same port.
* Discovery: UDP port 41799 — datagram `NDSP-DISCOVER/1` → JSON beacon
  `{service:"ndsp", protocol, name, port, fingerprint}`. **Discovery conveys
  location only, never trust.**
* Planned: QUIC/WebTransport with identical envelopes (`docs/ROADMAP.md`).

## Phases

### 1. Plaintext handshake (JSON text frames)

```
C→S  hello        {protocol, client{device_id,name,platform,app_version,
                   features?[]}, auth{method: pair | token+device_id},
                   codecs[]}
                   # features: optional capability flags.
                   #   "cursor"    → viewer renders the host cursor from
                   #                 cursor_shape/cursor_pos messages (host
                   #                 stops baking it into video while ALL
                   #                 connected viewers advertise it).
                   #   "clipboard" → viewer understands clipboard /
                   #                 clipboard_grant messages (still gated by
                   #                 the per-device grant on the host).
S→C  hello_ack    {protocol, server{name,app_version,fingerprint},
                   pairing_required, connection_nonce}      # 16B nonce, b64
C→S  pair_start   {client_pubkey, pake?}    # P-256, uncompressed SEC1, b64;
                                            # pake:true requests SPAKE2 pairing
S→C  pair_challenge {server_pubkey, salt, pake_share?}  # salt: 16B; pake_share
                                            # = SPAKE2 pB iff PAKE is in use
```

Both sides compute `shared = ECDH(eph_c, eph_s)` and
`session_key = HKDF-SHA256(ikm=shared, salt, info="ndsp-session-v1"‖nonce)`.

**PAKE pairing path** (first contact; host displays a PIN; v0.5+ default):

SPAKE2 over P-256 with the RFC 9382 `M`/`N` points:

```
w  = OS2IP(SHA-256("ndsp-pake-w-v1" ‖ lp(PIN) ‖ lp(salt) ‖ lp(nonce))) mod n  (0→1)
pB = y·G + w·N                                  (server, sent in pair_challenge)
pA = x·G + w·M                                  (client, sent in pair_confirm)
Z  = x·(pB − w·N) = y·(pA − w·M)
TT = SHA-256("ndsp-pake-v1" ‖ lp(nonce) ‖ lp(salt) ‖ lp(client_ecdh_pub)
             ‖ lp(server_ecdh_pub) ‖ lp(pA) ‖ lp(pB) ‖ lp(Z) ‖ lp(w))
pair_key = HKDF-SHA256(ikm=TT, salt, info="ndsp-pair-pake-v1"‖nonce)

C→S  pair_confirm {sealed, pake_share: pA}  # sealed = AES-GCM(pair_key,
                                            #   "ndsp-confirm-v1"‖nonce)
S→C  pair_result  {ok, sealed_token?}       # 32B trust token, sealed under
                                            #   pair_key (AAD "token")
```

`lp(x)` is a u16-BE length prefix; points travel as uncompressed SEC1.
The PIN never crosses the wire and — unlike the legacy path — **a recorded
transcript cannot be brute-forced offline**: shares are uniformly random
points regardless of the PIN, and testing a guess requires solving CDH. An
active MITM gets exactly one online guess per connection; a wrong guess fails
the AEAD open, rotates the PIN and counts against the source IP. `TT` binds
both ephemeral ECDH keys, so key-substitution MITM also fails. Byte-level
vectors: `shared/protocol/src/pake.rs` ↔ `viewer/web/tests/pake-vectors.mjs`.

**Legacy pairing path** (pre-v0.5 clients; host accepts it only while
`legacy_pin_pairing = true` in config — see SECURITY.md for the trade-off):

```
pair_key = HKDF-SHA256(shared, salt, "ndsp-pair-v1"‖PIN‖nonce)
C→S  pair_confirm {sealed}    # AES-GCM(pair_key, "ndsp-confirm-v1"‖nonce)
S→C  pair_result  {ok, sealed_token?}
```

A wrong PIN fails the AEAD open on either path; the host rotates the PIN and
counts the failure against the source IP.

**Token path** (returning device):

```
C→S  token_proof {proof}   # b64 SHA-256(token ‖ nonce ‖ client_pub ‖ server_pub)
```

The proof binds the stored token to this exact handshake, so an active MITM
substituting ephemeral keys invalidates it. Clients additionally pin the
host `fingerprint` from pairing and refuse to send proofs to a changed host.

Finally:

```
S→C  auth_ok  {codec, mode{width,height,refresh_hz}, input_allowed,
               clipboard_allowed, audio_available}
     (or auth_err {error})
```

### 2. Encrypted session (binary frames)

Every message is an envelope:

```
[chan u8][counter u64 BE][AES-256-GCM ciphertext‖tag]
nonce = [direction u8][chan u8][0 u16][counter u64 BE]   (12 bytes)
AAD   = chan byte
direction: 0 = server→client, 1 = client→server
```

Counters are strictly monotonic per (direction, channel); receivers reject
regressions (replay protection — WS is ordered, so any violation is hostile
or a broken middlebox and the session ends).

Channels: `1` control (JSON), `2` video, `3` audio.

### Video framing (inside channel 2)

```
[codec u8][flags u8][seq u32][ts_us u64][w u16][h u16][payload…]
codec: 0 jpeg, 1 h264 (Annex-B), 2 hevc*, 3 av1*      (*negotiated, not emitted yet)
flags: bit0 = keyframe
ts_us: host-clock capture timestamp (for measured e2e latency)
```

### Audio framing (inside channel 3)

```
[codec u8][channels u8][seq u32][ts_us u64][sample_rate u32][payload…]
codec: 0 opus (RFC 6716)
```

The host emits 48 kHz stereo Opus in 20 ms frames (~96 kbps). Audio flows
only while the host's audio switch is on **and** the viewer sent
`set_audio {enabled:true}` — off by default on both ends, with a live
indicator + per-client mute in the host panel. Packet loss shows up as `seq`
gaps (viewers glitch briefly instead of accumulating delay); `ts_us` shares
the video clock for A/V-skew reasoning.

### Control messages (channel 1, JSON `{type: …}`)

| type | dir | purpose |
|---|---|---|
| `ping {t0_us}` / `pong {t0_us,t1_us}` | C→S / S→C | liveness + NTP-style clock sync |
| `set_profile {profile}` | C→S | office / video / drawing / gaming envelope |
| `set_input_mode {mode}` | C→S | view_only / touchpad / direct_touch / keyboard_mouse / drawing_tablet |
| `request_keyframe` | C→S | decoder resync |
| `input {events[]}` | C→S | batched input (below) |
| `stats {stats}` | C→S | fps/decode/queue/rtt/e2e/net/present-wait — drives adaptation + panel |
| `host_stats {stats}` | S→C | capture fps, encode/convert ms, capture age, seal+send ms, bitrate, drops |
| `input_grant {allowed}` | S→C | live grant change from the panel |
| `clipboard_grant {allowed}` | S→C | live clipboard-grant change from the panel |
| `clipboard {text}` | both | clipboard sync; only honored with the device's clipboard grant on (deny by default) and ≤ 256 KiB — see SECURITY.md |
| `set_audio {enabled}` | C→S | viewer opts in/out of the audio stream (channel 3) |
| `cursor_shape {width,height,hot_x,hot_y,rgba}` | S→C | host cursor image (b64 RGBA8), sent on change to "cursor" viewers |
| `cursor_pos {x,y,visible}` | S→C | host cursor moved (normalized coords); `visible:false` also = hide overlay (e.g. legacy client joined) |
| `mode_change {mode}` | S→C | resolution/refresh switch |
| `bye {reason}` | both | graceful close |
| `error {code,message}` | both | non-fatal report |

Input events (coordinates normalized 0..1 on the streamed surface):
`mouse_move{x,y}`, `mouse_button{button,pressed}` (0=L,1=M,2=R,3/4=X),
`wheel{dx,dy}`, `key{code,pressed,key?}` (W3C `KeyboardEvent.code` position
strings, plus the optional layout-aware `KeyboardEvent.key` value — the host
injects the *character* for printable keys when present, so an AZERTY viewer
types what it sees; positional fallback otherwise),
`touch{id,phase,x,y,pressure}`, `pen{phase,x,y,pressure,tilt_x,tilt_y}`,
`text{text}`.

## Versioning & compatibility

* `hello.protocol` / `hello_ack.protocol`: peers speak `min(client, server)`;
  below `MIN_PROTOCOL_VERSION` the server replies `auth_err`.
* JSON messages ignore unknown fields (tested), so v1 peers tolerate v1.x
  additions. Breaking changes (envelope layout, crypto, framing) bump the
  major version; hosts may keep speaking old majors during a deprecation
  window (policy in ROADMAP).
* Channel ids and codec ids are append-only registries.

## Reconnect/resume

Sessions are stateless beyond trust: a viewer that drops simply reconnects
with its token and requests a keyframe — sub-second resume in practice. No
stream state is persisted host-side.
