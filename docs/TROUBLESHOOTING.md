# Troubleshooting

Start with the control panel (`https://localhost:38470/` on the host) —
the **Status** and **Fix connection** cards diagnose most of this
automatically.

## Viewer can't reach the host

1. **Same network?** Host and viewer must be on the same LAN/subnet
   (guest Wi-Fi networks often isolate clients).
2. **Firewall**: allow inbound TCP 38470 + UDP 38471 for `nebula-host`
   (the installer does this; manual: see docs/SECURITY.md).
3. **Use the IP directly**: `https://<host-ip>:38470/view/` — the panel
   shows the exact URL. Certificate warning on first visit is expected for
   the self-signed cert; check the fingerprint against the panel if in
   doubt.
4. **Discovery finds nothing** (native viewers): some APs block broadcast.
   Use manual address / QR — discovery is a convenience, not a dependency.

## Pairing fails

* `Wrong PIN` — PINs are single-use and expire in 120s; generate a fresh
  one. Five wrong attempts burn the PIN (by design).
* `No active pairing PIN` — click **Pair a device** on the panel first.
* Device was revoked → it must pair again (its token is gone).

## Stream is choppy / laggy

* Check the viewer stats overlay (📊): high `rtt` → network problem; high
  `decode` → weak viewer device (switch profile to *Office*); host
  `encode`/`capture` high → host under load.
* 2.4GHz Wi-Fi is the usual culprit: move either side to 5GHz/Ethernet, or
  use USB tethering.
* The adaptive controller reduces quality under congestion automatically —
  a persistently low `q` value in the overlay *is* the diagnosis.

## Extend mode unavailable / no virtual monitor

* Panel shows `driver: not_detected` → install it:
  `installer\windows\install-driver.ps1 -TestSign` (admin), see
  host/windows-driver/README.md — including the honest signing
  requirements. Mirror mode always works without the driver.
* Repair a broken install: `install-driver.ps1 -Repair`.

## Input doesn't work

Input is **off by default per device**. Control panel → Devices → toggle
*Input* for that device (takes effect immediately, no reconnect needed).

## Audio missing

Audio is off by default (privacy). Enable it in panel Settings *and* tick
Audio on the viewer before connecting. Non-Windows hosts don't capture
audio (shown in Status).

## Service issues (installed mode)

```powershell
sc query NebulaDisplayHost          # state
sc start NebulaDisplayHost
Get-EventLog -LogName Application -Source NebulaDisplayHost -Newest 20
```

Console mode with verbose logs for debugging:
`nebula-host --source auto` with `RUST_LOG=debug`.

## Reset to factory

Delete the data directory (`%APPDATA%\nebuladisplay` /
`~/.config/nebuladisplay`): removes config, trust store (all pairings), and
the TLS certificate (viewers will see a new-certificate warning once).
