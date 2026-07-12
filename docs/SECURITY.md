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

* Pairing (default, `pair_pake`): **SPAKE2 over P-256** (RFC 9382 blinding
  constants) bound to a **single-use, TTL-limited PIN**. The exchanged
  points are PIN-blinded, so a **recorded transcript cannot be ground
  offline** — each PIN guess costs an EC Diffie–Hellman solve. Both the
  pairing confirmation key and the session key derive from the PAKE shared
  point with the full transcript (nonce ‖ X ‖ Y ‖ w) in the KDF info.
  Byte-compatibility between the Rust host and the web viewer (both crypto
  backends) is CI-tested against a real host.
* Pairing (legacy, `pair`, kept for older mobile viewers): ephemeral ECDH +
  `pair_key = HKDF(shared, salt, "ndsp-pair-v1"‖PIN‖nonce)`. The PIN never
  crosses the wire; an active MITM without it cannot complete pairing —
  but see limitation 1 below.
* Session transport: fresh per-connection key (PAKE- or ECDH-derived) →
  **AES-256-GCM** on every frame. Forward secrecy: recording traffic and
  later stealing the trust store does not decrypt past sessions.
* Reconnect: 256-bit per-device token; proof = SHA-256 over token + nonce +
  both ephemeral keys (transcript binding defeats key-substitution MITM).
  Clients pin the host fingerprint and refuse proofs to a changed host.
* Envelopes: per-direction/channel monotonic counters inside the GCM nonce +
  counter regression rejection = replay protection; channel byte is AAD.
* Trust store at rest: on Windows the host encrypts `devices.json` with
  **DPAPI** (current-user scope), so the file is useless off-machine or to
  other local users; elsewhere it is plaintext with mode 0600.

### Known cryptographic limitations (honest)

1. **Offline PIN grinding — legacy method only**: a passive attacker who
   records a *legacy* (`pair`) exchange can brute-force the 6-digit PIN
   offline against `pair_confirm`. The default `pair_pake` method closes
   this; the legacy method remains only until the Android/iOS viewers ship
   PAKE (they currently pair via the legacy path). Mitigations meanwhile:
   PINs are single-use, expire in 5 min, rotate on every failure, and
   pairing is rare.
2. **No TLS layer**: HTTP serving the web viewer JS is plaintext on the LAN —
   an active LAN attacker could tamper the *viewer code* before crypto starts
   (native viewers are immune). Documented trade-off; mitigations: QR/manual
   fingerprint display, native clients, planned self-signed-cert + fingerprint
   pinning option.
3. Trust tokens are stored **raw inside the (DPAPI-wrapped on Windows) store**
   on the host and raw on clients: proofs are keyed hashes, so the verifier
   needs the key. Host compromise already means screen compromise; client
   platforms' app-storage isolation guards the viewer side.

## Controls by threat

| Threat | Control |
|---|---|
| Silent takeover via discovery | Beacons carry no trust; pairing always required; discovery can be disabled (`--discovery-port 0`) |
| PIN brute force (online) | Per-IP lockout (5 tries / 5 min default), PIN rotates on failure, single-use |
| Stolen trust token file (client) | Token useless without matching device_id? No — token is the credential: **revoke from the panel**; tokens are per-device so revocation is surgical |
| Rogue host at same IP | Client-side fingerprint pinning (tested) |
| Input abuse | Input **denied by default** per device; grants are live-revocable; sessions enforce grant server-side on every event batch |
| Clipboard exfiltration | Clipboard sync **denied by default** per device; grants live-revocable in the panel; enforced server-side on every message; 256 KiB cap; contents never logged; a newly connected viewer never receives pre-existing clipboard contents |
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
* Clipboard sync is implemented **permission-gated per device** (deny by
  default, panel-toggled, live-revocable, size-capped). Nothing is shared
  implicitly: connecting viewers never see clipboard state from before they
  connected, and the origin viewer never receives its own push back. File
  transfer stays designed-but-unimplemented (protocol reserves space).

## Reporting

Security reports: open a GitHub issue titled `[security]` (private disclosure
contact can be added when the project has one).
