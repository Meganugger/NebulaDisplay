# NebulaDisplay security & threat model

## Principles

1. **Local-only by default.** No cloud, no accounts, no telemetry, no
   outbound connections. Screen content never leaves the LAN.
2. **Discovery ≠ trust.** Anyone on the LAN can *find* the host; nobody can
   *see or control* anything without interactive pairing.
3. **Least privilege, explicit consent.** Input injection, audio, and
   clipboard are separate, per-device / host-controlled grants, all off by
   default.
4. **Secrets are never stored in plaintext on the host.** Device tokens are
   stored as SHA-256 hashes; the plaintext exists only on the paired device.

## Assets & adversaries

**Assets:** live screen content (highest value — may contain anything),
input control of the host, audio, clipboard, device tokens, TLS key.

**Adversaries considered:**

| Adversary | Capabilities |
|---|---|
| A1: passive LAN attacker | sniffs traffic (open Wi-Fi, port mirroring) |
| A2: active LAN attacker | ARP/mDNS spoofing, MITM, connects to any port |
| A3: malicious/compromised viewer device | holds a valid token |
| A4: local unprivileged user on the host | can read user files, hit localhost APIs |
| A5: shoulder surfer | sees the PIN on screen |

## Controls

### Transport (vs A1/A2)
* TLS on by default with a persistent self-signed certificate; key stored
  user-readable-only (0600 / per-user %APPDATA% ACL).
* The certificate's SHA-256 fingerprint is surfaced on the control panel,
  in discovery replies, and in QR payloads. Native viewers **pin** it
  (Android/iOS reject a mismatching host before sending anything);
  browsers rely on the standard certificate prompt on first visit.
* `--no-tls` exists for loopback testing and is clearly flagged in
  diagnostics and discovery replies as insecure.

### Pairing & authentication (vs A2/A3/A5)
* Pairing requires a **single-use 6-digit PIN** displayed only on the host,
  valid 120s, max 5 wrong attempts (then burned), constant-time compare.
  A5 who reads the PIN must also beat the legitimate user to consuming it —
  and revocation is one click.
* Success issues a per-device **256-bit random token**; the host stores its
  hash only. Tokens are revocable individually on the control panel.
* Token verification is constant-time over hashes; unknown `device_id`s
  and bad tokens receive identical errors.

### Authorization (vs A3)
* Streaming requires an authenticated session — there is no anonymous mode.
* **Input injection is denied by default** and granted per device by the
  host user; the grant is re-checked on every input batch, so revocation is
  immediate. Same model planned for clipboard (master switch, off).
* `max_clients` bounds resource use; heartbeats reap dead sessions.

### Host-local surface (vs A4)
* The admin API (`/api/admin/*`: PIN issuance, trust management, config) is
  **loopback-only**, enforced on the connection's source address.
  Note: this protects against remote attackers, not against other local
  processes of the same user — the classic local-desktop boundary.
* The service needs no elevation to mirror (DXGI); the driver runs in user
  mode (UMDF) under the reflector, so a driver crash cannot BSOD.

### Logging
* Logs contain connection metadata, sizes, and timings — never frame
  payloads, PINs, or tokens (`Debug` on the trust store never prints them).

## Firewall guidance

Allow inbound **TCP 38470** (stream/panel) and **UDP 38471** (discovery)
for `nebula-host.exe`, **private/domain profiles only** — the installer
creates exactly these rules and removes them on uninstall. Do not forward
these ports through your router; NDSP has no internet mode.

## Known residual risks (honest list)

* **Browser first-visit warning**: users may click through the self-signed
  cert prompt without checking the fingerprint (A2 window on first ever
  visit). Mitigation: QR flow embeds host+fingerprint; native viewers pin.
* **Same-user local processes** (A4) can call the loopback admin API.
  OS-level per-process isolation is out of scope for a user-mode app.
* **UMDF shared memory section** is created in `Global\` with default
  SYSTEM/Admin+user access; a hardened SDDL is a planned tightening.
* **Clipboard/file transfer** are protocol-complete but the OS bridging is
  intentionally not wired yet — the permission gates ship first.

## Future: remote-over-internet mode (design constraints)

If ever built, it must be: opt-in per host, end-to-end encrypted with keys
that never touch a relay (Noise/WireGuard-style), rendezvous-only server,
and visually distinct from local mode. Until those hold, it stays unbuilt.

## Reporting

Open a GitHub security advisory (private) on the repository. Please do not
file public issues for exploitable problems.
