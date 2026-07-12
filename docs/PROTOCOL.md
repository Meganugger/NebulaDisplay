# NDSP — NebulaDisplay Stream Protocol v1

Status: implemented (Rust host + Rust/web/Kotlin/Swift clients).
Authority: `shared/protocol` — this document describes what that code does.

## Transport

* WebSocket (`ws://host:41800/ndsp`), binary subprotocol described below.
  With `https = true` in the host config the same endpoint is `wss://` behind
  a persisted self-signed certificate (fingerprint shown host-side).
* Plain HTTP (or HTTPS) serves the web viewer statics on the same port.
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
                   # features: optional capability flags. "cursor" → this
                   # viewer renders the host cursor from cursor_shape/
                   # cursor_pos messages (host stops baking it into video
                   # while ALL connected viewers advertise it).
S→C  hello_ack    {protocol, server{name,app_version,fingerprint},
                   pairing_required, connection_nonce}      # 16B nonce, b64
C→S  pair_start   {client_pubkey, pake_share?}              # P-256, uncompressed SEC1, b64
S→C  pair_challenge {server_pubkey, salt, pake_share?}      # salt: 16B
```

Both sides compute `shared = ECDH(eph_c, eph_s)`.

**Pairing path** (first contact; host displays a PIN). Modern clients run a
balanced **PAKE** (CPace pattern over ristretto255, `shared/protocol/src/pake.rs`):

```
G = ristretto255_map(SHA-512("ndsp-pake-v1" ‖ len(PIN) u8 ‖ PIN ‖ nonce))
A = a·G (client pake_share)      B = b·G (server pake_share)
K = a·B = b·A                                        # 32B, canonical encoding
pair_key    = HKDF-SHA256(shared ‖ K, salt, "ndsp-pair-pake-v1"‖nonce)
session_key = HKDF-SHA256(shared ‖ K, salt, "ndsp-session-pake-v1"‖nonce)
C→S  pair_confirm {sealed}    # AES-GCM(pair_key, "ndsp-confirm-v1"‖nonce)
S→C  pair_result  {ok, sealed_token?}   # 32B trust token, sealed under pair_key (AAD "token")
```

The PIN never crosses the wire **and the transcript cannot be ground offline**
— testing a candidate PIN against a recording requires solving CDH in the
group. A wrong PIN fails the AEAD open; the host rotates the PIN and counts
the failure against the source IP. Clients that offered a `pake_share` hard-
fail if the challenge omits one (no downgrade). Legacy clients (no
`pake_share`) still pair via the v1 schedule
(`pair_key = HKDF(shared, salt, "ndsp-pair-v1"‖PIN‖nonce)`,
`session_key = HKDF(shared, salt, "ndsp-session-v1"‖nonce)`) unless the host
sets `require_pake = true`.

**Token path** (returning device — no PIN, so no PAKE; v1 session-key
schedule `HKDF(shared, salt, "ndsp-session-v1"‖nonce)`):

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

Channels: `1` control (JSON), `2` video, `3` audio (Opus), `4` file drop.

### Video framing (inside channel 2)

```
[codec u8][flags u8][seq u32][ts_us u64][w u16][h u16][payload…]
codec: 0 jpeg, 1 h264 (Annex-B), 2 hevc (Annex-B, hardware hosts), 3 av1*
flags: bit0 = keyframe                                 (*negotiated, not emitted yet)
ts_us: host-clock capture timestamp (for measured e2e latency)
```

### Audio framing (inside channel 3)

Sent only after the session opted in with `set_audio` and the host replied
`audio_start`. Same clock as video timestamps (lip-sync capable).

```
[codec u8][flags u8][seq u32][ts_us u64][payload…]
codec: 0 opus (48 kHz stereo, 20 ms packets)
seq:   wrapping packet counter (gap ⇒ conceal)
```

### File chunks (inside channel 4)

Sent only after the receiver's user explicitly accepted the corresponding
`file_offer` (host panel). Offsets are strictly sequential per transfer.

```
[transfer_id u32][offset u64][data…]
```

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
| `clipboard_data {format,data}` | both | clipboard sync (`format:"text"`, `data` b64 UTF-8); dropped without a grant / over the size cap |
| `clipboard_grant {allowed}` | S→C | live clipboard-grant change from the panel |
| `file_offer {transfer_id,name,size,sha256}` | C→S | offer a dropped file; nothing streams until accepted |
| `file_accept {transfer_id}` / `file_reject {transfer_id,reason}` | S→C | host user's explicit panel decision |
| `file_done {transfer_id,ok,error?}` | S→C | receiver verdict (SHA-256 verified before `ok`) |
| `set_audio {enabled}` | C→S | opt in/out of host audio (host config gates it; `error {code:"audio_disabled"}` if off) |
| `audio_start {codec,sample_rate,channels}` / `audio_stop` | S→C | audio stream lifecycle on channel 3 |
| `cursor_shape {width,height,hot_x,hot_y,rgba}` | S→C | host cursor image (b64 RGBA8), sent on change to "cursor" viewers |
| `cursor_pos {x,y,visible}` | S→C | host cursor moved (normalized coords); `visible:false` also = hide overlay (e.g. legacy client joined) |
| `mode_change {mode}` | S→C | resolution/refresh switch |
| `bye {reason}` | both | graceful close |
| `error {code,message}` | both | non-fatal report |

Input events (coordinates normalized 0..1 on the streamed surface):
`mouse_move{x,y}`, `mouse_button{button,pressed}` (0=L,1=M,2=R,3/4=X),
`wheel{dx,dy}`, `key{code,pressed,key?}` (W3C `KeyboardEvent.code` physical
position, plus the layout-resolved `KeyboardEvent.key` character when there
is one — the host prefers the scancode and falls back to the character for
mismatched layouts), `touch{id,phase,x,y,pressure}`,
`pen{phase,x,y,pressure,tilt_x,tilt_y}` (injected as Windows Ink with real
pressure/tilt when the host supports it), `text{text}`, and
`gamepad{buttons,left_x,left_y,right_x,right_y,left_trigger,right_trigger}`
(W3C standard-mapping snapshot; `buttons` is a bitmask by button index).

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
