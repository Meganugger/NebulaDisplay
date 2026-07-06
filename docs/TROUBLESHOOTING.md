# Troubleshooting

## "Fix connection" checklist (in order)

1. **Same network?** Host and viewer must be on the same LAN/subnet for
   discovery; manual `ip:port` works across routed subnets.
2. **Host running?** `http://<host-ip>:41800/healthz` should print `ok` from
   the viewer device's browser.
3. **Firewall.** Allow `nebulad` on *private* networks: TCP 41800 (viewer) and
   UDP 41799 (discovery). The installer's optional task adds exactly these.
   Manual: `netsh advfirewall firewall add rule name="NebulaDisplay Viewer" dir=in action=allow protocol=TCP localport=41800 profile=private`
4. **AP/client isolation.** Many guest/hotel/office Wi-Fi networks block
   device-to-device traffic. Use a private network, a phone hotspot, or USB.
5. **PIN rejected?** PINs are single-use and expire in 5 minutes — read the
   current one from the panel (`http://127.0.0.1:41888/panel.html` on the
   host). After 5 failures your device's IP is locked out for 5 minutes.
6. **"Host identity changed" warning.** The host reinstalled (new identity
   key) — or something is impersonating it. If you expect the change (fresh
   install), pair again with the PIN; the stale trust entry was cleared.

## Symptoms

| Symptom | Likely cause → fix |
|---|---|
| Viewer page loads but "cannot reach host" on connect | WS blocked: corporate proxy/VPN intercepting port 41800 → try another port (`--port`), or USB mode |
| Black/frozen stream, stats show 0 fps decoded | Decoder stall → toolbar Stats on, check `dropped`; the viewer auto-requests keyframes; reconnect if persistent |
| Very high `e2e` latency, low `rtt` | Client decode too slow (old phone/browser) → Office profile, lower host `--capture-size`, or native viewer |
| High `rtt` + climbing latency | Wi-Fi congestion/bufferbloat — adaptation cuts bitrate automatically; move to 5 GHz/Ethernet for 60 fps |
| Stream fine, input does nothing | Input is **denied by default** → host panel → Trusted devices → Allow input; also pick an input mode in the viewer toolbar |
| Keys type wrong characters | Non-US host layout: NDSP sends physical key codes (positional) — matching layouts on host/guest, layout-aware mapping is on the roadmap |
| `AcquireNextFrame` errors in logs on UAC prompts | Expected: secure desktop pauses mirror capture; stream resumes automatically |
| No extend mode, only mirror | Driver not installed/signed/devnode absent → `installer/install-driver.ps1 -Repair`, check Device Manager → Display adapters → "NebulaDisplay Virtual Display", Event Viewer → DriverFrameworks-UserMode |
| Discovery finds nothing | UDP 41799 blocked or `--discovery-port 0` → manual IP connect always works |
| Panel unreachable | It's loopback-only by design — open it on the host machine itself |

## Logs

* Host: stderr, controlled by `RUST_LOG` (e.g. `RUST_LOG=debug nebulad`).
  Logs contain metadata only — never screen pixels, PINs, or keys.
* Web viewer: browser devtools console.
* Driver: Event Viewer → Applications and Services → Microsoft → Windows →
  DriverFrameworks-UserMode.

## Getting help

Open a GitHub issue with: host OS/GPU, viewer platform/browser, network type,
`RUST_LOG=debug` host log around the failure, and the stats overlay values.
