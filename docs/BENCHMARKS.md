# Benchmarks

All numbers are **measured** by the instrumentation the product ships (host
per-stage counters + viewer synced-clock latency), through the reproducible
harness:

```
cargo build --release -p nebulad
cd viewer/web && npm run build
node tests/bench.mjs            # full matrix, ~2 min per row
node tests/bench.mjs --quick    # one row, CI-friendly
```

## Environment for the recorded run (2026-07-06)

* 4 **shared** vCPUs (cloud sandbox), no GPU, headless Chromium
  (software decode + software canvas), loopback network.
* Source: synthetic test pattern = **full-frame motion every frame** — the
  *worst case* for the encoder and for dirty-region elision (a real desktop
  is mostly static and encodes far less).
* Encoder: OpenH264 software (hardware encoders are roadmap P0.2).

This environment underestimates real hardware substantially (no GPU decode,
no GPU canvas, shared CPU). Treat the numbers as *relative* and re-run on
deployment hardware for absolute ones.

## Matrix (loopback, full-frame motion, software everything)

| size | profile | fps | e2e ms | net+host ms | present ms | enc ms | cvt ms | age ms | send ms | dec ms |
|---|---|---|---|---|---|---|---|---|---|---|
| 1280x720 | office (30 cap) | 29.0 | 41.6 | 19.2 | 5.8 | 9.6 | 2.3 | 9.0 | 0.0 | 1.2 |
| 1280x720 | video (60 cap) | 54.8 | 23.6 | 15.1 | 5.7 | 9.5 | 2.2 | 5.0 | 0.0 | 1.1 |
| 1920x1080 | video | 25.3 | 44.2 | 19.7 | 9.5 | 19.1 | 5.6 | 0.1 | 0.0 | 1.7 |
| 1920x1080 | gaming | 25.2 | 61.3 | 20.6 | 11.8 | 19.3 | 5.7 | 0.2 | 0.0 | 3.0 |
| 2560x1440 | video | 14.3 | 78.8 | 34.2 | 14.7 | 34.0 | 9.7 | 0.1 | 0.1 | 2.6 |
| 3840x2160 | video | 6.4 | 159.8 | 75.9 | 31.2 | 71.8 | 21.1 | 0.4 | 0.1 | 5.9 |

Column meanings: `e2e` = capture timestamp → canvas paint (synced clocks);
`net+host` = capture → envelope arrival at the viewer; `present` = decode
completion → paint (includes the actual software-canvas draw, ~4–5 ms
headless — <1 ms on GPU-composited browsers); `enc` includes `cvt`
(color-convert share); `age` = capture → encode-start wait (≈ half the
capture-to-target-fps phase offset — 0 when encode keeps up with capture).

**Reading it:** on this CPU the software encoder is the wall from 1080p up
(19–72 ms/frame ⇒ fps collapses and e2e inflates behind it). Everything
around the encoder — seal+send 0.1 ms, decode 1–6 ms, scheduling ~0 — is
already thin. This is exactly why hardware encoders (Media Foundation →
NVENC/QuickSync/AMF) are roadmap P0.2: they move 1440p/4K60 from impossible
to routine on real machines, with the rest of this pipeline already able to
carry it.

## Before → after (this overhaul, same harness/hardware)

| Metric | before | after |
|---|---|---|
| Browser E2E, 720p office | 58.4 ms @ 30.1 fps | 41.6 ms @ 29 fps (10 s avg) |
| Browser E2E, 720p video profile | *stuck at ~29 fps* (profile-switch bug) | 54.8 fps @ 23.6 ms |
| 1080p software encode (full-motion microbench) | 27.9 ms/frame | 22.6 ms/frame (multi-slice) |
| Encoder rebuilds during bitrate adaptation | on **every raise** (IDR storm: "runtime bitrate update failed") | **zero** (regression-tested) |
| Static desktop (idle) | full encode+send per frame | **no encode, no send** (dirty elision) |
| Cursor move over static desktop | GPU readback + full encode + full frame sent | ~40-byte control message (cursor channel) |
| Video frame copies host-side | capture→convert→encode→**concat copy→encrypt copy**→send | capture→convert(dirty rows only)→encode→**in-place seal**→send |
| Web presentation | painted on next rAF (0–16.7 ms queue; stalls when throttled) | painted at decode (microtask-coalesced) |

## What cannot be measured in this environment (and how to measure it)

| Scenario | Why not here | How |
|---|---|---|
| LAN / Wi-Fi / Ethernet deltas | loopback only | run `bench.mjs` with `NEBULAD_BIN` on the host machine and open the printed URL from the device under test; the same overlay numbers apply |
| Motion-to-photon / touch latency | needs a camera + physical screen | 240 fps phone camera on both screens; count frames between physical event and remote update |
| Android / iOS / Windows viewers | no devices/OS here | stats overlays in those viewers report the same measured fields |
| Extend mode (IddCx ring) | driver needs WDK + signing | after driver install, `nebulad` logs the active source; same overlay metrics |
| Hardware decode | headless Chromium is SW-only | any real browser: `chrome://media-internals` shows the decoder; overlay `dec` drops accordingly |
| GPU/CPU/RAM usage | shared sandbox is unrepresentative | Task Manager / `perfmon` alongside a bench run |
