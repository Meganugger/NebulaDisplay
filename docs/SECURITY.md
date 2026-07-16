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

1. **Offline PIN grinding — fixed for SPAKE2 clients**: the web, desktop
   (v0.5) and **Android (v0.6)** viewers pair with SPAKE2, whose transcript
   a passive recorder cannot grind (the Android implementation is proven
   byte-compatible with the Rust reference by a CI interop exchange). The
   *legacy* scheme (still spoken by the current iOS app) keeps the old
   caveat: a recorded legacy pairing exchange can be brute-forced offline
   against the 6-digit PIN. Mitigations: PINs are single-use, expire in
   5 min, rotate on every failure, pairing is rare — and hosts can refuse
   the legacy scheme entirely (`allow_legacy_pairing = false`). Remaining
   work: SPAKE2 on iOS, then flip the default off.
2. **TLS is opt-in, not default**: plain-HTTP serving of the web viewer JS
   is tamperable by an active LAN attacker before crypto starts (native
   viewers are immune). v0.5 adds `--https`: a **persistent self-signed
   certificate** (fingerprint in the panel/banner — compare once, pinned
   thereafter) which also unlocks secure-context browser features
   (WebCodecs, clipboard API) on LAN addresses. Plain HTTP stays the default
   because self-signed warnings hurt first-run UX; hostile-LAN users should
   turn `--https` on.
3. Trust tokens at rest: on **Windows they are DPAPI-wrapped (v0.5)**; on
   **Linux/macOS (v0.6) they are sealed with AES-256-GCM under a wrapping
   key held in the OS keychain** (Secret Service / macOS Keychain) — in
   all three cases another local account or an exfiltrated copy of the
   files cannot read them (`host/service/src/keystore.rs`; also covers the
   identity key and the TLS private key). Headless Unix systems without a
   keychain daemon fall back to 0600 plaintext (logged loudly);
   `NDSP_NO_KEYCHAIN=1` opts out explicitly (headless boxes / CI, where a
   locked macOS keychain would block waiting for an interactive unlock). Proofs are
   keyed hashes, so the verifier needs the key material; host compromise
   already means screen compromise.
4. **QUIC TLS certificates are not verified by clients** — deliberately.
   The QUIC transport (v0.6) needs a TLS cert to exist (protocol
   requirement); the host presents its persistent self-signed cert and
   native viewers skip verification. This is *not* a weakening relative to
   `ws://` (which has no TLS at all): every NDSP guarantee — mutual
   PIN-bound pairing, fingerprint pinning, transcript-bound token proofs,
   AES-256-GCM envelopes — lives above the transport and holds identically
   on both. A QUIC MITM learns exactly what a TCP MITM learns: the public
   handshake JSON and ciphertext.

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
| Malicious file drop | Every transfer needs an **explicit per-file accept in the panel**; filenames sanitized to one path component; size caps; sha256 verified; partial/failed transfers deleted; offers expire in 120 s |
| Host→viewer file send abuse | Viewer-side explicit accept (web dialog; desktop only with `--receive-dir`); sanitized names, size caps, sha256 verification, spool files deleted on every outcome |
| Replay/reorder injection | Envelope counters + GCM |
| Panel exposure | Panel binds 127.0.0.1 only; contains PIN/grants; never reachable from LAN |
| Driver attack surface | Driver has no network code; validates geometry; ring is `Local\` namespace |
| Log leakage | Logs carry metadata only (sizes, timings, device names) — never pixels, tokens, PINs, or key material |

## Service design / least privilege

* `nebulad` runs as the logged-on user (it must read the user's desktop
  anyway); it does **not** require or request elevation.
* Viewer endpoint binds `0.0.0.0:41800` (configurable; TCP for HTTP/WS and
  UDP for QUIC on the same number); panel `127.0.0.1:41888`.
* Firewall guidance: allow TCP 41800 + UDP 41800 (QUIC) + UDP 41799
  (discovery) on **private** profiles only (the installer's optional rules
  do exactly this and nothing more).
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

## Reporting

Security reports: open a GitHub issue titled `[security]` (private disclosure
contact can be added when the project has one).
