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
                   features?[]},
                   auth{method: pair_spake2 | pair | token+device_id},
                   codecs[]}
                   # features: optional capability flags. "cursor" → this
                   # viewer renders the host cursor from cursor_shape/
                   # cursor_pos messages (host stops baking it into video
                   # while ALL connected viewers advertise it).
S→C  hello_ack    {protocol, server{name,app_version,fingerprint},
                   pairing_required, connection_nonce}      # 16B nonce, b64
```

**SPAKE2 pairing path** (`pair_spake2` — the current scheme; first contact,
host displays a PIN). A real PAKE: the transcript is *not* offline-grindable
against the PIN, and authentication is mutual. Construction in
`shared/protocol/src/spake2.rs` (RFC-9382-style over P-256; deterministic
nothing-up-my-sleeve `M`/`N`; `w = scalar(PIN, nonce)`):

```
C→S  spake2_start     {share}   # pA = x·G + w·M, uncompressed SEC1, b64
S→C  spake2_challenge {share}   # pB = y·G + w·N
C→S  spake2_confirm   {mac}     # HMAC-SHA256(KcA, transcript) — proves PIN
S→C  spake2_result    {ok, mac?, sealed_token?, error?}
                                # mac  = HMAC(KcB, transcript) — server's
                                #        proof; client verifies BEFORE trusting
                                # token sealed under the SPAKE2 token key
session_key = HKDF(Ke, "ndsp-spake2-session-v1")   # fresh per connection
```

**Legacy pairing path** (`pair`; kept for viewers without curve arithmetic —
the current mobile apps — and host-disableable via
`allow_legacy_pairing = false`). Ephemeral ECDH first:

```
C→S  pair_start   {client_pubkey}                           # P-256, uncompressed SEC1, b64
S→C  pair_challenge {server_pubkey, salt}                   # salt: 16B
shared      = ECDH(eph_c, eph_s)
session_key = HKDF-SHA256(ikm=shared, salt, info="ndsp-session-v1"‖nonce)
pair_key    = HKDF-SHA256(shared, salt, "ndsp-pair-v1"‖PIN‖nonce)
C→S  pair_confirm {sealed}    # AES-GCM(pair_key, "ndsp-confirm-v1"‖nonce)
S→C  pair_result  {ok, sealed_token?}   # 32B trust token, sealed under pair_key (AAD "token")
```

In both schemes the PIN never crosses the wire. A wrong PIN fails the
MAC/AEAD check; the host rotates the PIN and counts the failure against the
source IP. (The legacy transcript is offline-grindable by a passive
recorder — the documented reason SPAKE2 replaced it; see SECURITY.md.)

**Token path** (returning device; preceded by the same `pair_start` /
`pair_challenge` ECDH exchange, which yields the session key):

```
C→S  token_proof {proof}   # b64 SHA-256(token ‖ nonce ‖ client_pub ‖ server_pub)
```

The proof binds the stored token to this exact handshake, so an active MITM
substituting ephemeral keys invalidates it. Clients additionally pin the
host `fingerprint` from pairing and refuse to send proofs to a changed host.

Finally:

```
S→C  auth_ok  {codec, mode{width,height,refresh_hz}, input_allowed}
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

Off by default. A viewer opts in with `set_audio`; the host streams only
while the device's audio permission (panel-toggleable) also allows it.

```
[codec u8][channels u8][seq u32][ts_us u64][sample_rate u32][payload…]
codec: 0 opus, 1 pcm_s16le (interleaved)
```

Fixed pipeline format: 48 kHz stereo in 10 ms blocks. `ts_us` shares the
video clock (lip-sync + latency measurement). `pcm_s16le` exists for web
viewers on insecure origins, which have no WebCodecs Opus decoder
(≈1.5 Mbit/s — trivial on a LAN).

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
| `cursor_shape {width,height,hot_x,hot_y,rgba}` | S→C | host cursor image (b64 RGBA8), sent on change to "cursor" viewers |
| `cursor_pos {x,y,visible}` | S→C | host cursor moved (normalized coords); `visible:false` also = hide overlay (e.g. legacy client joined) |
| `mode_change {mode}` | S→C | resolution/refresh switch |
| `set_audio {enabled, codec?}` | C→S | opt into host audio (`opus` / `pcm`); **off by default** |
| `audio_grant {allowed}` | S→C | live audio-permission change from the panel (mute) |
| `clipboard {text}` | both | clipboard sync; only honored with the device's clipboard grant; ≤ 256 KiB |
| `clipboard_grant {allowed}` | S→C | live clipboard-permission change |
| `file_offer {id,name,size_bytes,sha256}` | C→S | offer a file; host queues an explicit panel accept |
| `file_answer {id,accept,reason?}` | S→C | the user's panel decision |
| `file_chunk {id,seq,data}` | C→S | in-order b64 chunk, ≤ 256 KiB raw |
| `file_end {id}` | C→S | sender done → host verifies size + sha256 |
| `file_done {id}` | S→C | verified, moved into the receive directory |
| `file_abort {id,reason}` | both | cancel / verification failure (partial file deleted) |
| `bye {reason}` | both | graceful close |
| `error {code,message}` | both | non-fatal report |

Input events (coordinates normalized 0..1 on the streamed surface):
`mouse_move{x,y}`, `mouse_button{button,pressed}` (0=L,1=M,2=R,3/4=X),
`wheel{dx,dy}`, `key{code,pressed,key?}` (W3C `KeyboardEvent.code` position
plus the optional layout-resolved `KeyboardEvent.key` character — hosts
prefer `key` for printables so viewer keyboard layouts survive the trip),
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
