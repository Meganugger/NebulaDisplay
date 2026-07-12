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

* Pairing (first contact, default): **SPAKE2 (RFC 9382) over P-256** bound
  to the single-use, TTL-limited PIN (`shared/protocol/src/pake.rs`). The
  PIN never crosses the wire, an active MITM without it cannot complete
  pairing, and — the point of the PAKE — **a recorded transcript reveals
  nothing to grind the PIN against offline**: testing a guess requires an
  ephemeral discrete log. `K = xy·G` also gives pairing exchanges forward
  secrecy against a later PIN leak.
* Pairing (legacy, for older clients): ephemeral **ECDH P-256** + PIN-bound
  **HKDF-SHA256** (`"ndsp-pair-v1"`). Kept only for pre-PAKE viewers
  (current Android/iOS apps) and can be disabled host-side with
  `allow_legacy_pair = false` — see limitation 1 below.
* Session keys: fresh per connection (from the PAKE transcript or ECDH) →
  **AES-256-GCM**. Forward secrecy: recording traffic and later stealing the
  trust store does not decrypt past sessions.
* Reconnect: 256-bit per-device token; proof = SHA-256 over token + nonce +
  both ephemeral keys (transcript binding defeats key-substitution MITM).
  Clients pin the host fingerprint and refuse proofs to a changed host.
* Envelopes: per-direction/channel monotonic counters inside the GCM nonce +
  counter regression rejection = replay protection; channel byte is AAD.
* Optional HTTPS (`https = true` / `--https`): the viewer endpoint serves
  over TLS with a **persistent self-signed certificate**; its SHA-256 is
  printed at startup and shown in the panel. The desktop viewer can pin it
  (`--tls-fingerprint`, SSH-known-hosts model: exactly that cert or abort).

### Known cryptographic limitations (honest)

1. **Legacy pairing is grindable**: while `allow_legacy_pair = true` (the
   default, for older mobile viewers), a passive attacker recording a
   *legacy* pairing can brute-force the 6-digit PIN offline against
   `pair_confirm`. Mitigations: PINs are single-use, expire in 5 min,
   rotate on every failure, pairing is rare — and all in-repo web/desktop
   viewers already use SPAKE2. Set `allow_legacy_pair = false` for
   PAKE-only operation.
2. **Plain-HTTP viewer code** (default): without `https = true`, an active
   LAN attacker could tamper the *viewer page code* before crypto starts
   (native viewers are immune). Enable HTTPS + fingerprint verification on
   hostile LANs; browsers will show a one-time self-signed warning.
3. Trust tokens on the **host** are stored raw at rest (0600): proofs are
   keyed hashes, so the verifier needs the key, and host compromise already
   means screen compromise. The **desktop viewer** seals its tokens with
   DPAPI on Windows (current-user scope); Unix clients use 0600 JSON.
   Keychain/libsecret integration remains roadmap.

## Controls by threat

| Threat | Control |
|---|---|
| Silent takeover via discovery | Beacons carry no trust; pairing always required; discovery can be disabled (`--discovery-port 0`) |
| PIN brute force (online) | Per-IP lockout (5 tries / 5 min default), PIN rotates on failure, single-use |
| Stolen trust token file (client) | Token useless without matching device_id? No — token is the credential: **revoke from the panel**; tokens are per-device so revocation is surgical |
| Rogue host at same IP | Client-side fingerprint pinning (tested) |
| Input abuse | Input **denied by default** per device; grants are live-revocable; sessions enforce grant server-side on every event batch |
| Clipboard exfiltration | Clipboard sync **denied by default** per device; both directions gated + size-capped (256 KiB); nothing on the clipboard at connect time is ever sent (only changes made afterwards) |
| Covert audio listening | Audio capture **off by default** host-side; even when on, each viewer must opt in per session; the panel shows a live 🔊 indicator per listening viewer |
| Viewer-code tampering on hostile LANs | Optional HTTPS with a persistent self-signed cert; fingerprint printed/pinned (`--tls-fingerprint` on the desktop viewer) |
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
* Audio capture (WASAPI loopback → Opus) is **off by default** (`audio =
  false`); when enabled, streaming to a viewer additionally requires that
  viewer's explicit opt-in, and the panel shows a live 🔊 indicator for every
  listening session.
* Clipboard sync is **permission-gated per device (deny by default)** with a
  256 KiB size cap; only clipboard *changes made while connected and granted*
  are shared — never the pre-existing clipboard content.
* File transfer: designed permission-gated, not yet implemented — the
  protocol reserves message space; nothing is shared implicitly.

## Reporting

Security reports: open a GitHub issue titled `[security]` (private disclosure
contact can be added when the project has one).
