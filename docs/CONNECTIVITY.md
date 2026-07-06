# Connectivity Modes

NDSP is transport-agnostic TCP/UDP on the local segment; every mode below is
just a different way to get IP connectivity between host and viewer. **No
internet is ever required.**

## Ethernet / Wi-Fi LAN (primary)

Host and viewer on the same network. Discovery (UDP 41799 broadcast) fills the
host list automatically; otherwise type `ip:41800` or scan the panel QR.
Recommendation for 1080p60: wired host + 5 GHz viewer, or both wired.

## Wi-Fi Direct / hotspot

No router? Make one:

* **Windows hotspot**: Settings → Network → Mobile hotspot → enable. Connect
  the viewer device to it; the host is reachable at the hotspot gateway IP
  (usually `192.168.137.1`), so: `192.168.137.1:41800`.
* **Phone hotspot**: connect *both* PC and second device to the phone's AP.

## USB — Android (zero-config with adb)

Wired, lowest jitter, charges the device:

```bash
# on the PC (host), phone in developer mode:
adb reverse tcp:41800 tcp:41800
adb reverse tcp:41888 tcp:41888    # optional: phone can read the panel too
```

Then connect the Android viewer (or Chrome) to `127.0.0.1:41800`. `adb
reverse` tunnels the phone's localhost to the PC over USB — no root, no
special app permissions. USB tethering (Settings → Hotspot → USB tethering)
also works without adb: the PC gets a `192.168.42.x` address; use the phone's
gateway address from the PC's new interface.

## USB — iOS/iPadOS

Enable *Personal Hotspot* and plug in the cable: iOS exposes an
Ethernet-over-USB interface to the PC (via Apple Mobile Device Service /
iTunes drivers). The PC appears on `172.20.10.x`; connect the viewer to the
PC's address on that interface. A usbmuxd-native transport (no hotspot
needed) is specced in ROADMAP.

## Manual / QR connect

* Manual: any viewer accepts `host[:port]`.
* QR: the host panel renders `http://<ip>:41800/?pin=<PIN>&fp=<fingerprint>`
  — scanning opens the web viewer pre-filled and auto-pairs once.

## Remote over internet (deliberately not built-in)

NebulaDisplay refuses to open itself to the internet. If you need remote use,
tunnel with something designed for it — WireGuard/Tailscale — and treat the
tunnel as your LAN. The threat model (docs/SECURITY.md) assumes hostile LANs,
so this composes safely; a first-class, opt-in rendezvous design is sketched
in ROADMAP and will remain separate from local mode.

## Ports

| Port | Proto | Purpose | Exposure |
|---|---|---|---|
| 41800 | TCP | viewer HTTP + NDSP WS | LAN (configurable) |
| 41799 | UDP | discovery probes | LAN, optional |
| 41888 | TCP | control panel | 127.0.0.1 only |
