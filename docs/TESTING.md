# Testing NebulaDisplay

## Automated (run in CI and locally)

| Layer | Command | What it proves |
|---|---|---|
| Protocol unit tests | `cargo test -p nebula-proto` | round-trips, frozen wire layouts, forward-compat (unknown fields/types), version negotiation |
| Host unit tests | `cargo test -p nebula-host --lib` | pairing lifecycle & rate limiting, token hashing/revocation, dirty-rect correctness (incl. edge tiles), JPEG validity, adaptive AIMD behavior, pipeline full-refresh, config defaults, TLS material persistence |
| Host integration | `cargo test -p nebula-host --test e2e` | real WS client: hello → refused-before-auth → wrong PIN → pair → stream → decodable JPEG frames → input gating → pong/stats → token reconnect → bad-token rejection; admin API |
| Web typecheck | `cd viewer/web && npm run typecheck` | strict TS across viewer/panel/protocol |
| Browser E2E | `node tests/browser-smoke.mjs` | real host + real viewer in Chromium: PIN pairing, live animating canvas, stats overlay, token reconnect; screenshots in `tests/artifacts/` |
| Windows cross-check | `cargo check --target x86_64-pc-windows-msvc --no-default-features` | all cfg(windows) code compiles against the real Windows API surface |

## Manual test matrix (release gate)

Legend: ✅ automated here · 🖐 needs hardware · — n/a

| Axis | Cases | Status |
|---|---|---|
| Host OS | Windows 10 22H2, Windows 11 24H2 | 🖐 |
| Link | Ethernet, Wi-Fi 5GHz, Wi-Fi 2.4GHz, phone hotspot, USB tether (Android RNDIS), adb reverse | 🖐 (protocol is link-agnostic; tested on loopback ✅) |
| Resolution | 720p / 1080p ✅ (loopback) / 1440p / 4K 🖐 |
| FPS target | 30 ✅ / 60 / 90 / 120 🖐 (profile bounds unit-tested ✅) |
| Modes | mirror ✅ (test source), extend 🖐 (needs driver on hardware) |
| Input | off-by-default ✅, grant/revoke live ✅, mouse/keyboard/touch/stylus injection 🖐 (SendInput code type-checked ✅) |
| Audio | off-by-default ✅, WASAPI capture 🖐 |
| Degraded network | packet loss / high RTT — see below |
| Sleep/wake, monitor hot-plug | duplication auto-recreate path 🖐 |
| Multi-client | 2–4 simultaneous viewers 🖐 (per-client pipelines ✅ by design) |

## Degraded-network testing (Linux)

```bash
# 100ms ± 20ms delay, 2% loss, 20Mbit shaping on loopback port 38470:
sudo tc qdisc add dev lo root handle 1: prio
sudo tc qdisc add dev lo parent 1:3 handle 30: netem delay 100ms 20ms loss 2%
sudo tc filter add dev lo parent 1:0 protocol ip u32 match ip dport 38470 0xffff flowid 1:3
# run the browser smoke test / a manual session, watch the stats overlay:
#   expected: quality/fps step down within ~1s, recover within ~5s of removal
sudo tc qdisc del dev lo root
```

Expected adaptive behavior (unit-tested in `adaptive.rs`): multiplicative
decrease on congestion signals, floor at profile minimums (never to zero),
additive recovery only after 2s of health, decode-limited clients get FPS
caps rather than quality loss.

## Soak

`nebula-host --source test` + a browser viewer left overnight; watch for
memory growth (expected flat: bounded queues, no per-frame allocs beyond
buffers) and `frames_dropped` monotony on the panel.

## What remains manual and why

Real capture (DXGI), input injection, audio loopback, and the IddCx driver
need Windows hardware/WDK which this repo's dev sandbox lacks. Every one of
those paths is (a) type-checked against the msvc target in CI and (b)
isolated behind traits with tested non-Windows implementations, so the
untested surface is the thinnest possible layer of OS calls.
