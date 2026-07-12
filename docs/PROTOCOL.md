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
                   features?[]}, auth{method: pair_pake | pair | token+device_id},
                   codecs[]}
                   # features: optional capability flags. "cursor" → this
                   # viewer renders the host cursor from cursor_shape/
                   # cursor_pos messages (host stops baking it into video
                   # while ALL connected viewers advertise it).
S→C  hello_ack    {protocol, server{name,app_version,fingerprint},
                   pairing_required, connection_nonce}      # 16B nonce, b64
C→S  pair_start   {client_pubkey}                           # P-256, uncompressed SEC1, b64
S→C  pair_challenge {server_pubkey, salt}                   # salt: 16B
```

The `pair_start`/`pair_challenge` points and key derivation depend on the
auth method; the message flow is identical for all three.

**PAKE pairing path** (`pair_pake` — default for current clients; host
displays a PIN): SPAKE2 over P-256 with the RFC 9382 blinding constants
M/N. `w = int(HKDF(ikm=PIN, salt=nonce, info="ndsp-pake-w-v1")) mod n`
(0 maps to 1):

```
pair_start.client_pubkey    X = x·G + w·M
pair_challenge.server_pubkey Y = y·G + w·N
Z = x·(Y − w·N) = y·(X − w·M)          # abort on identity/off-curve
pair_key    = HKDF(ikm=Z_uncompressed, salt, "ndsp-pake-pair-v1"‖nonce‖X‖Y‖w)
session_key = HKDF(ikm=Z_uncompressed, salt, "ndsp-pake-session-v1"‖nonce‖X‖Y‖w)
C→S  pair_confirm {sealed}    # AES-GCM(pair_key, "ndsp-confirm-v1"‖nonce)
S→C  pair_result  {ok, sealed_token?}   # 32B trust token, sealed under pair_key (AAD "token")
```

A recorded `pair_pake` transcript is **not offline-grindable** — each PIN
guess requires solving EC Diffie–Hellman.

**Legacy pairing path** (`pair` — kept for older mobile viewers):
`shared = ECDH(eph_c, eph_s)`,
`session_key = HKDF-SHA256(ikm=shared, salt, "ndsp-session-v1"‖nonce)`,

```
pair_key = HKDF-SHA256(shared, salt, "ndsp-pair-v1"‖PIN‖nonce)
C→S  pair_confirm {sealed}    # AES-GCM(pair_key, "ndsp-confirm-v1"‖nonce)
S→C  pair_result  {ok, sealed_token?}   # 32B trust token, sealed under pair_key (AAD "token")
```

Under either method the PIN never crosses the wire. A wrong PIN fails the
AEAD open; the host rotates the PIN and counts the failure against the
source IP.

**Token path** (returning device): plain ephemeral ECDH
(`session_key = HKDF(shared, salt, "ndsp-session-v1"‖nonce)`) plus:

```
C→S  token_proof {proof}   # b64 SHA-256(token ‖ nonce ‖ client_pub ‖ server_pub)
```

The proof binds the stored token to this exact handshake, so an active MITM
substituting ephemeral keys invalidates it. Clients additionally pin the
host `fingerprint` from pairing and refuse to send proofs to a changed host.

Finally:

```
S→C  auth_ok  {codec, mode{width,height,refresh_hz}, input_allowed,
               clipboard_allowed}
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

Channels: `1` control (JSON), `2` video, `3` audio (reserved).

### Video framing (inside channel 2)

```
[codec u8][flags u8][seq u32][ts_us u64][w u16][h u16][payload…]
codec: 0 jpeg, 1 h264 (Annex-B), 2 hevc*, 3 av1*      (*negotiated, not emitted yet)
flags: bit0 = keyframe
ts_us: host-clock capture timestamp (for measured e2e latency)
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
| `clipboard {text}` | both | clipboard text sync — only honored under an explicit per-device grant; ≤ 256 KiB, oversize answered with `error {code:"clipboard_too_large"}` |
| `clipboard_grant {allowed}` | S→C | live clipboard-grant change from the panel |
| `cursor_shape {width,height,hot_x,hot_y,rgba}` | S→C | host cursor image (b64 RGBA8), sent on change to "cursor" viewers |
| `cursor_pos {x,y,visible}` | S→C | host cursor moved (normalized coords); `visible:false` also = hide overlay (e.g. legacy client joined) |
| `mode_change {mode}` | S→C | resolution/refresh switch |
| `bye {reason}` | both | graceful close |
| `error {code,message}` | both | non-fatal report |

Input events (coordinates normalized 0..1 on the streamed surface):
`mouse_move{x,y}`, `mouse_button{button,pressed}` (0=L,1=M,2=R,3/4=X),
`wheel{dx,dy}`, `key{code,pressed,key?}` (`code` = W3C `KeyboardEvent.code`
physical position; optional `key` = layout-resolved `KeyboardEvent.key`
character so the host can inject correctly across mismatched keyboard
layouts — see `host/service/src/input/windows_inject.rs`),
`touch{id,phase,x,y,pressure}`, `pen{phase,x,y,pressure,tilt_x,tilt_y}`
(injected as a true Windows Ink pen with pressure/tilt/hover where the host
supports synthetic pointers), `text{text}`.

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
