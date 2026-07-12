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
* Pairing (preferred path): **NDSP-PAKE v1**, a CPace-style balanced PAKE on
  P-256 — the PIN is hashed into a fresh group generator (RFC 9380
  hash-to-curve) and both sides run Diffie-Hellman on it, with channel
  binding to the connection nonce, device id and host fingerprint. The PIN
  never crosses the wire, **and a recorded transcript cannot be brute-forced
  offline**: every PIN guess costs a CDH instance. An active guesser gets
  one online attempt per connection (rate-limited; PIN rotates on failure).
  Interop between the Rust host and the web viewer is pinned by an RFC 9380
  test vector on both stacks + a full cross-stack handshake test in CI.
* Pairing (legacy path, for not-yet-updated mobile viewers): session bound
  to the single-use, TTL-limited PIN via
  `pair_key = HKDF(shared, salt, "ndsp-pair-v1"‖PIN‖nonce)`. Subject to the
  offline-grinding caveat below; hosts can refuse it entirely with
  `allow_legacy_pairing = false`.
* Reconnect: 256-bit per-device token; proof = SHA-256 over token + nonce +
  both ephemeral keys (transcript binding defeats key-substitution MITM).
  Clients pin the host fingerprint and refuse proofs to a changed host.
* Envelopes: per-direction/channel monotonic counters inside the GCM nonce +
  counter regression rejection = replay protection; channel byte is AAD.
* At rest: the host trust store and the desktop viewer's credentials are
  **DPAPI-protected on Windows** (user-scope OS keystore; a copied file is
  useless on another machine/account) and mode-0600 on unix.
* Optional TLS (`tls = true`): the viewer endpoint serves HTTPS/WSS behind a
  per-install self-signed certificate. Native clients pin its SHA-256
  fingerprint (`--tls-pin` / `Transport::TlsPinned`); browsers accept it
  once. This protects the *web viewer code* against on-path tampering on
  hostile LANs — NDSP's own encryption never depended on it.

### Known cryptographic limitations (honest)

1. **Offline PIN grinding — legacy pairing only**: a passive attacker who
   records a *legacy* pairing exchange can brute-force the 6-digit PIN
   offline against `pair_confirm`. The PAKE path (used by the web viewer,
   desktop viewer and client SDK) is immune; the legacy path remains
   accepted by default only for the Android/iOS apps until they ship PAKE
   (set `allow_legacy_pairing = false` to refuse it). Legacy mitigations
   remain: PINs are single-use, expire in 5 min, rotate on every failure.
2. **Web viewer code delivery**: with TLS off (the default), the HTTP page
   serving the viewer JS is plaintext on the LAN — an active LAN attacker
   could tamper the *viewer code* before crypto starts (native viewers are
   immune). Mitigations: the optional TLS mode above, QR/manual fingerprint
   display, native clients.
3. macOS Keychain / Linux secret-service backends for at-rest credential
   protection are not yet wired (0600 files there; DPAPI on Windows is).

## Controls by threat

| Threat | Control |
|---|---|
| Silent takeover via discovery | Beacons carry no trust; pairing always required; discovery can be disabled (`--discovery-port 0`) |
| PIN brute force (online) | Per-IP lockout (5 tries / 5 min default), PIN rotates on failure, single-use |
| Stolen trust token file (client) | Token useless without matching device_id? No — token is the credential: **revoke from the panel**; tokens are per-device so revocation is surgical |
| Rogue host at same IP | Client-side fingerprint pinning (tested) |
| Input abuse | Input **denied by default** per device; grants are live-revocable; sessions enforce grant server-side on every event batch |
| Clipboard exfiltration | Clipboard sync **denied by default** per device; grants are live-revocable; 256 KiB per-event cap; nothing is pushed to a newly connected device (changes only); enforced server-side both directions |
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
* Clipboard sync is **off by default** and permission-gated per device (panel
  toggle, live-revocable); only explicit changes are forwarded, text only,
  size-capped. File transfer remains unimplemented (protocol space reserved);
  nothing is shared implicitly.

## Reporting

Security reports: open a GitHub issue titled `[security]` (private disclosure
contact can be added when the project has one).
