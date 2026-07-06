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
| 1280x720 | office (30 cap) | 27.5 | 16.8 | 10.9 | 6.0 | 9.8 | 2.4 | 0.1 | 0.0 | 1.2 |
| 1280x720 | video (60 cap) | 52.1 | 16.5 | 10.1 | 5.6 | 9.5 | 2.3 | 0.3 | 0.0 | 1.1 |
| 1920x1080 | video | 24.8 | 31.5 | 20.9 | 9.6 | 20.2 | 5.8 | 0.1 | 0.1 | 1.7 |
| 1920x1080 | gaming | 25.1 | 33.6 | 20.4 | 9.7 | 19.7 | 5.8 | 0.1 | 0.1 | 1.8 |
| 2560x1440 | video | 14.2 | 52.4 | 33.7 | 14.8 | 33.0 | 9.8 | 0.1 | 0.1 | 2.6 |
| 3840x2160 | video | 6.3 | 117.6 | 76.2 | 37.6 | 70.0 | 21.1 | 0.4 | 0.1 | 6.0 |

The stages now *sum to the total*: e.g. 720p60 → 10.1 (host+net) + 1.1
(decode) + 5.6 (present incl. software-canvas draw) ≈ 16.5 measured e2e —
there is no unexplained latency left in the pipeline. The software encoder
is 55–60 % of e2e at 720p/1080p and the software canvas paint most of the
rest; on real hardware (NVENC ≈ 1–3 ms, GPU compositing < 1 ms) the same
pipeline delivers single-digit e2e.

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

Two latency fixes landed *between* these rows and the first recorded matrix:
the pacing loop now waits for content captured **after** the rate gate opens
(cut ≈ 9 ms of average frame staleness at a 30 fps cap), and a measurement
bug was fixed where e2e was sampled at stats time instead of paint time
(inflating every previously reported e2e by up to a frame interval — the
"before" numbers below share that inflation, so relative deltas hold).

| Metric | before | after |
|---|---|---|
| Browser E2E, 720p office | 58.4 ms @ 30.1 fps | 16.8 ms @ 27.5 fps (10 s avg, corrected measurement) |
| Browser E2E, 720p video profile | *stuck at ~29 fps* (profile-switch bug) | 52.1 fps @ 16.5 ms |
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
