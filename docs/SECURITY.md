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

* Handshake: ephemeral **ECDH P-256** per connection → **HKDF-SHA256** →
  **AES-256-GCM** session key. Forward secrecy: recording traffic and later
  stealing the trust store does not decrypt past sessions.
* Pairing: the session is bound to a **single-use, TTL-limited PIN** via
  `pair_key = HKDF(shared, salt, "ndsp-pair-v1"‖PIN‖nonce)`. The PIN never
  crosses the wire; an active MITM without it cannot complete pairing.
* Reconnect: 256-bit per-device token; proof = SHA-256 over token + nonce +
  both ephemeral keys (transcript binding defeats key-substitution MITM).
  Clients pin the host fingerprint and refuse proofs to a changed host.
* Envelopes: per-direction/channel monotonic counters inside the GCM nonce +
  counter regression rejection = replay protection; channel byte is AAD.

### Known cryptographic limitations (honest)

1. **Offline PIN grinding**: a passive attacker who records a *pairing*
   exchange can brute-force the 6-digit PIN offline against `pair_confirm`.
   Mitigations today: PINs are single-use, expire in 5 min, rotate on every
   failure, and pairing is rare. Fix planned: **PAKE (SPAKE2/CPace)** so the
   transcript is un-grindable (ROADMAP P1).
2. **No TLS layer**: HTTP serving the web viewer JS is plaintext on the LAN —
   an active LAN attacker could tamper the *viewer code* before crypto starts
   (native viewers are immune). Documented trade-off; mitigations: QR/manual
   fingerprint display, native clients, planned self-signed-cert + fingerprint
   pinning option.
3. Trust tokens are stored **raw** at rest (0600) on host and clients: proofs
   are keyed hashes, so the verifier needs the key. Host compromise already
   means screen compromise; still, an OS-keystore upgrade is planned.

## Controls by threat

| Threat | Control |
|---|---|
| Silent takeover via discovery | Beacons carry no trust; pairing always required; discovery can be disabled (`--discovery-port 0`) |
| PIN brute force (online) | Per-IP lockout (5 tries / 5 min default), PIN rotates on failure, single-use |
| Stolen trust token file (client) | Token useless without matching device_id? No — token is the credential: **revoke from the panel**; tokens are per-device so revocation is surgical |
| Rogue host at same IP | Client-side fingerprint pinning (tested) |
| Input abuse | Input **denied by default** per device; grants are live-revocable; sessions enforce grant server-side on every event batch |
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
* Audio capture is **off** and unimplemented until the WASAPI feature lands —
  it will be opt-in per device with a visible indicator (ROADMAP).
* Clipboard/file transfer: designed permission-gated, not yet implemented —
  the protocol reserves message space; nothing is shared implicitly.

## Reporting

Security reports: open a GitHub issue titled `[security]` (private disclosure
contact can be added when the project has one).
