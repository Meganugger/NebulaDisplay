# NebulaDisplay Security Model & Threat Analysis

Principles: **local-first** (no cloud, no accounts), **encrypted by default**,
**deny input by default**, **no telemetry**, **screen content never logged**.

## Assets

1. Screen content (highest value — may contain anything the user sees).
2. Input injection capability (full host takeover if abused).
3. Trust tokens / pairing PIN.
4. Availability of the user's desktop (a crashing driver is a DoS of the PC).

## Trust boundaries

```
[Internet] ✂ (nothing listens; LAN only by default)
[LAN peers] — untrusted until paired; can see discovery beacons + ciphertext
[Paired viewers] — may watch the stream; input only after explicit grant
[Host machine] — trusted (it renders the screen in the first place)
[Driver] — no network access; only fills a local shared-memory ring
```

## Cryptography (implemented, tested)

* Pairing (current): **SPAKE2 over P-256** (RFC-9382-style; see
  `shared/protocol/src/spake2.rs`) bound to a **single-use, TTL-limited
  PIN**. The PIN never crosses the wire, the recorded transcript is **not
  offline-grindable**, and authentication is **mutual** (the host proves PIN
  knowledge back to the client before any token is trusted). Fresh
  ephemerals per connection → forward secrecy.
* Pairing (legacy, mobile apps): PIN-bound HKDF over ephemeral ECDH.
  Disableable host-side (`allow_legacy_pairing = false`); see limitation 1.
* Reconnect handshake: ephemeral **ECDH P-256** per connection →
  **HKDF-SHA256** → **AES-256-GCM** session key. Forward secrecy: recording
  traffic and later stealing the trust store does not decrypt past sessions.
* Reconnect: 256-bit per-device token; proof = SHA-256 over token + nonce +
  both ephemeral keys (transcript binding defeats key-substitution MITM).
  Clients pin the host fingerprint and refuse proofs to a changed host.
* Envelopes: per-direction/channel monotonic counters inside the GCM nonce +
  counter regression rejection = replay protection; channel byte is AAD.

### Known cryptographic limitations (honest)

1. **Offline PIN grinding — fixed for SPAKE2 clients (v0.5)**: the web and
   desktop viewers now pair with SPAKE2, whose transcript a passive recorder
   cannot grind. The *legacy* scheme (still spoken by the current Android/
   iOS apps) keeps the old caveat: a recorded legacy pairing exchange can be
   brute-forced offline against the 6-digit PIN. Mitigations: PINs are
   single-use, expire in 5 min, rotate on every failure, pairing is rare —
   and hosts can refuse the legacy scheme entirely
   (`allow_legacy_pairing = false`). Remaining work: SPAKE2 in the mobile
   apps, then flip the default off.
2. **TLS is opt-in, not default**: plain-HTTP serving of the web viewer JS
   is tamperable by an active LAN attacker before crypto starts (native
   viewers are immune). v0.5 adds `--https`: a **persistent self-signed
   certificate** (fingerprint in the panel/banner — compare once, pinned
   thereafter) which also unlocks secure-context browser features
   (WebCodecs, clipboard API) on LAN addresses. Plain HTTP stays the default
   because self-signed warnings hurt first-run UX; hostile-LAN users should
   turn `--https` on.
3. Trust tokens at rest: on **Windows they are DPAPI-wrapped (v0.5)** —
   another local account or an exfiltrated copy of the files cannot read
   them (`host/service/src/keystore.rs`; also covers the identity key and
   the TLS private key). On Unix they remain 0600 plaintext (no
   universally-present keystore daemon). Proofs are keyed hashes, so the
   verifier needs the key material; host compromise already means screen
   compromise.

## Controls by threat

| Threat | Control |
|---|---|
| Silent takeover via discovery | Beacons carry no trust; pairing always required; discovery can be disabled (`--discovery-port 0`) |
| PIN brute force (online) | Per-IP lockout (5 tries / 5 min default), PIN rotates on failure, single-use |
| Stolen trust token file (client) | Token useless without matching device_id? No — token is the credential: **revoke from the panel**; tokens are per-device so revocation is surgical |
| Rogue host at same IP | Client-side fingerprint pinning (tested) |
| Input abuse | Input **denied by default** per device; grants are live-revocable; sessions enforce grant server-side on every event batch |
| Clipboard theft/poisoning | Clipboard sync **denied by default** per device; 256 KiB cap both ways; host→viewer flow only polls the OS clipboard while a granted device is connected, and never ships pre-session clipboard content |
| Covert listening | Audio is **per-viewer opt-in** (never streams unrequested); the panel shows a live "🔊 listening" indicator per device and can mute any device instantly; capture stops (device released) at zero listeners |
| Malicious file drop | Every transfer needs an **explicit per-file accept by a human on the receiving side** (host panel for viewer→host, on-screen viewer prompt for host→viewer); filenames sanitized to one path component; size caps; sha256 verified; partial/failed transfers deleted; offers expire in 120 s |
| Replay/reorder injection | Envelope counters + GCM |
| Panel exposure | Panel binds 127.0.0.1 only; contains PIN/grants; never reachable from LAN |
| Driver attack surface | Driver has no network code; validates geometry; ring is `Local\` namespace |
| Log leakage | Logs carry metadata only (sizes, timings, device names) — never pixels, tokens, PINs, or key material |

## Service design / least privilege

* `nebulad` runs as the logged-on user (it must read the user's desktop
  anyway); it does **not** require or request elevation.
* Viewer endpoint binds `0.0.0.0:41800` (configurable); panel `127.0.0.1:41888`.
* Firewall guidance: allow TCP 41800 + UDP 41799 on **private** profiles only
  (the installer's optional rules do exactly this and nothing more).
* The driver is the only elevated component (installed once, admin), and is
  isolated from all protocol/network code.

## Privacy

* No telemetry, no crash uploads, no update phone-home. Nothing leaves the LAN
  unless the user configures it.
* Audio capture (v0.5) is **off by default**: it starts only when a viewer
  explicitly enables it *and* the panel permits that device, shows a live
  per-device listening indicator, and the capture device is released the
  moment the last listener stops.
* Clipboard sync (v0.5) is **deny-by-default per device**, size-capped, and
  never ships clipboard content that predates the session.
* File drop (v0.5) writes nothing without an explicit per-transfer accept in
  the panel. Nothing is shared implicitly.
* Host→viewer file send is panel-initiated by **uploading the picked file's
  bytes** — the service deliberately exposes no "send an arbitrary host
  path to a viewer" API that another local process could abuse for
  exfiltration — and the viewer must explicitly accept before any bytes
  flow (sha256-verified on arrival).

## Reporting

Security reports: open a GitHub issue titled `[security]` (private disclosure
contact can be added when the project has one).
