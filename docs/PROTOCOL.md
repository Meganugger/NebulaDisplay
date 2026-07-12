# NDSP — NebulaDisplay Stream Protocol v1

Status: implemented (Rust host + Rust/web/Kotlin/Swift clients).
Authority: `shared/protocol` — this document describes what that code does.

## Transport

* WebSocket (`ws://host:41800/ndsp`), binary subprotocol described below.
* Plain HTTP serves the web viewer statics on the same port.
* Optional TLS (`tls = true` on the host): the same endpoint becomes
  `https://` + `wss://` behind a per-install self-signed certificate whose
  SHA-256 fingerprint is printed at startup and shown in the panel. Native
  clients authenticate the server **only** by that pinned fingerprint
  (`Transport::TlsPinned` in the client SDK / `--tls-pin` in the desktop
  viewer); browsers must accept the certificate once. NDSP's own encryption
  is unchanged — TLS additionally protects the *web viewer code* on hostile
  LANs.
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
                   #   "cursor"    → this viewer renders the host cursor from
                   #                 cursor_shape/cursor_pos messages (host
                   #                 stops baking it into video while ALL
                   #                 connected viewers advertise it).
                   #   "clipboard" → this viewer understands clipboard sync
                   #                 messages (still gated by the per-device
                   #                 grant).
S→C  hello_ack    {protocol, server{name,app_version,fingerprint},
                   pairing_required, connection_nonce, pake?}
                   # connection_nonce: 16B, b64.
                   # pake: PAKE suite for pairing ("p256-v1"); clients that
                   # understand it use pake_start below, legacy clients
                   # ignore the field.
```

**Pairing path — PAKE (preferred; host displays a PIN):**

```
C→S  pake_start     {client_pubkey}          # Ya, uncompressed SEC1, b64
S→C  pake_challenge {server_pubkey, salt}    # Yb + 16B HKDF salt
```

NDSP-PAKE v1 is a CPace-style balanced PAKE on P-256
(`shared/protocol/src/pake.rs` is the authority):

```
g   = hash_to_curve(P256_XMD:SHA-256_SSWU_RO_,
                    DST = "NDSP-PAKE-V1-P256_XMD:SHA-256_SSWU_RO_",
                    msg = lp(PIN) ‖ lp(nonce) ‖ lp(device_id) ‖ lp(fingerprint))
Ya  = a·g   Yb = b·g          (a, b random in [1, n-1])
K   = a·Yb = b·Ya
ISK = SHA-256("ndsp-pake-isk-v1" ‖ lp(nonce) ‖ lp(Ya) ‖ lp(Yb) ‖ lp(x(K)))
pair_key    = HKDF-SHA256(ikm=ISK, salt, "ndsp-pair-v1" ‖ nonce)
session_key = HKDF-SHA256(ikm=ISK, salt, "ndsp-session-v1" ‖ nonce)
```

(`lp` = 2-byte big-endian length prefix.) A **passive transcript cannot be
ground offline against the PIN** — each guess requires solving a
Diffie-Hellman instance. An active guesser gets one online attempt per
connection, rate-limited, and the PIN rotates on failure. Then:

```
C→S  pair_confirm {sealed}    # AES-GCM(pair_key, "ndsp-confirm-v1"‖nonce)
S→C  pair_result  {ok, sealed_token?}   # 32B trust token, sealed under pair_key (AAD "token")
```

**Pairing path — legacy PIN-HKDF** (pre-PAKE clients; host accepts it unless
`allow_legacy_pairing = false`):

```
C→S  pair_start   {client_pubkey}                           # P-256, uncompressed SEC1, b64
S→C  pair_challenge {server_pubkey, salt}                   # salt: 16B
     shared      = ECDH(eph_c, eph_s)
     pair_key    = HKDF-SHA256(shared, salt, "ndsp-pair-v1"‖PIN‖nonce)
     session_key = HKDF-SHA256(shared, salt, "ndsp-session-v1"‖nonce)
C→S  pair_confirm / S→C pair_result     # exactly as above
```

Either way the PIN never crosses the wire. A wrong PIN fails the AEAD open;
the host rotates the PIN and counts the failure against the source IP.

**Token path** (returning device; uses the `pair_start` ECDH exchange for
the fresh session key):

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
| `clipboard_grant {allowed}` | S→C | live clipboard-grant change from the panel |
| `clipboard {text}` | both | clipboard text sync — only honored with the per-device clipboard grant (deny by default); ≤ 256 KiB per event (oversized events are refused with `error{code:"clipboard_too_large"}`, never truncated) |
| `cursor_shape {width,height,hot_x,hot_y,rgba}` | S→C | host cursor image (b64 RGBA8), sent on change to "cursor" viewers |
| `cursor_pos {x,y,visible}` | S→C | host cursor moved (normalized coords); `visible:false` also = hide overlay (e.g. legacy client joined) |
| `mode_change {mode}` | S→C | resolution/refresh switch |
| `bye {reason}` | both | graceful close |
| `error {code,message}` | both | non-fatal report |

Input events (coordinates normalized 0..1 on the streamed surface):
`mouse_move{x,y}`, `mouse_button{button,pressed}` (0=L,1=M,2=R,3/4=X),
`wheel{dx,dy}`, `key{code,pressed,key?}` (`code` = W3C `KeyboardEvent.code`
physical key; optional `key` = the layout-resolved character when it is a
single printable char — the host resolves it against **its own** layout via
`VkKeyScanW`, so typing and shortcuts stay correct across mismatched
keyboard layouts; chars needing AltGr on the host are injected as Unicode),
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
