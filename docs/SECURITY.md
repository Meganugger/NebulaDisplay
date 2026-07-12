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
* Pairing: a balanced **PAKE** (CPace pattern over ristretto255) binds the
  exchange to the **single-use, TTL-limited PIN**: both sides derive a
  PIN-bound group generator and mix the PAKE secret into the HKDF alongside
  the ECDH secret (`docs/PROTOCOL.md` has the exact schedule). The PIN never
  crosses the wire; an active MITM without it gets exactly **one online
  guess** per connection (rate-limited), and a recorded transcript is
  **not offline-grindable** — testing a candidate PIN requires solving CDH.
  Clients refuse to downgrade if a server omits its PAKE share.
* Reconnect: 256-bit per-device token; proof = SHA-256 over token + nonce +
  both ephemeral keys (transcript binding defeats key-substitution MITM).
  Clients pin the host fingerprint and refuse proofs to a changed host.
* Envelopes: per-direction/channel monotonic counters inside the GCM nonce +
  counter regression rejection = replay protection; channel byte is AAD.

### Known cryptographic limitations (honest)

1. **Legacy pairing compatibility**: pre-PAKE viewers may still pair using
   the v1 PIN-in-HKDF schedule, whose recorded transcript *is* offline-
   grindable. All first-party viewers in this repo use the PAKE; set
   `require_pake = true` in `config.toml` to refuse legacy pairing entirely
   once every device is updated.
2. **Plain-HTTP viewer serving (default)**: the web viewer JS is fetched in
   plaintext on the LAN — an active LAN attacker could tamper the *viewer
   code* before crypto starts (native viewers are immune). Mitigation
   implemented: set `https = true` to serve over TLS with a persisted
   self-signed certificate; the host prints/panels the SHA-256 fingerprint
   to compare against the browser's one-time warning. This also gives the
   browser a secure context (native WebCrypto/WebCodecs).
3. Trust tokens must be stored recoverably (proofs are keyed hashes, so the
   verifier needs the key). At rest they are **DPAPI-protected on Windows**
   (per-user; the file is useless off-machine or under another account) and
   0600 plaintext on unix hosts. macOS Keychain / Android Keystore
   integration for the mobile viewers remains planned.

## Controls by threat

| Threat | Control |
|---|---|
| Silent takeover via discovery | Beacons carry no trust; pairing always required; discovery can be disabled (`--discovery-port 0`) |
| PIN brute force (online) | Per-IP lockout (5 tries / 5 min default), PIN rotates on failure, single-use |
| Stolen trust token file (client) | Token useless without matching device_id? No — token is the credential: **revoke from the panel**; tokens are per-device so revocation is surgical |
| Rogue host at same IP | Client-side fingerprint pinning (tested) |
| Input abuse | Input **denied by default** per device; grants are live-revocable; sessions enforce grant server-side on every event batch |
| Clipboard exfiltration | Clipboard sync **denied by default** per device (panel toggle); size-capped both directions; origin-tagged (never echoed back); text-only |
| Malicious file drops | Nothing is written until the host user **explicitly accepts each transfer** in the panel; names sanitized to one path component; size caps at offer and during streaming; full SHA-256 verification before the file appears under its real name |
| Covert audio listening | Host audio is **off by default** (`audio = false`); per-session opt-in; the panel shows a live 🔊 indicator for every listening session |
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
* Audio (WASAPI loopback → Opus) is **off by default** in config; when
  enabled, each session must still opt in and the panel shows a visible
  indicator per listening device. Microphones are never touched — loopback
  captures what the speakers play.
* Clipboard sync and file drop are permission-gated exactly like input:
  deny-by-default per device, live-revocable, nothing shared implicitly.

## Reporting

Security reports: open a GitHub issue titled `[security]` (private disclosure
contact can be added when the project has one).
