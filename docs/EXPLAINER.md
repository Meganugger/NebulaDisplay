# NebulaDisplay — explainer

*A guided tour of what was built, why it is shaped this way, and how to
verify it. Written for readers ranging from "never touched a streaming
system" to "reviews Windows drivers for breakfast" — skip sections you
already know.*

---

## Background

### The problem space (deep background — skippable)

"Use my tablet as a second monitor" hides three genuinely different
problems in one sentence:

1. **Making the OS believe a monitor exists.** Windows only renders a
   desktop for displays its graphics stack knows about. To *extend* the
   desktop (rather than copy an existing screen), something must present a
   monitor to Windows. Since Windows 10 1607, the sanctioned way is an
   **Indirect Display Driver (IddCx)**: a *user-mode* driver that tells
   Windows "a monitor arrived", receives the composed frames for it via a
   swap-chain, and does whatever it wants with them. Before IddCx, vendors
   used fragile kernel mirror drivers; IddCx made the whole category safe —
   a buggy driver just restarts instead of blue-screening.

2. **Getting pixels efficiently.** For *mirroring* an existing screen no
   driver is needed at all: the **DXGI Desktop Duplication API** hands any
   app the exact frames the GPU presents, with change metadata. For
   *extended* virtual monitors, the IddCx swap-chain is the source.

3. **Moving pixels with low latency.** A 1080p60 BGRA stream is ~4 Gbit/s
   raw — compression is mandatory. The whole engineering game is the
   *latency budget*: capture + encode + network + decode + present, and
   what to do when any stage falls behind (spoiler: drop, never queue).

### This codebase in one paragraph

NebulaDisplay is a monorepo with a Rust **host service** (`crates/nebula-host`)
that captures frames (IddCx shared memory → real virtual monitor, or DXGI →
mirror, or a synthetic test pattern anywhere), detects the changed screen
region per frame, JPEG-encodes just that rectangle, and ships it over a
TLS WebSocket using a small versioned protocol (**NDSP**, `crates/nebula-proto`).
Viewers — a browser app (`viewer/web`), Android (Kotlin), iOS (Swift), and
a Tauri desktop shell — decode and composite the rectangles, send input
back, and report health so the host's adaptive controller can react. A C++
IddCx driver (`host/windows-driver`) provides the real virtual monitor.

---

## Intuition

Three ideas carry the design; everything else is plumbing.

> 💡 **Idea 1: send only what changed.**
> A desktop is mostly static. Split each frame into 64×64 tiles, hash each
> tile, compare with the previous frame, and encode only the bounding
> rectangle of changed tiles. Toy example: you type one character in a
> terminal on a 1920×1080 screen → exactly one tile changes → we JPEG ~64×64
> pixels (~1 KB) instead of 1920×1080 (~200 KB). That's the entire trick
> behind "MJPEG is fine, actually" for office content — with a safety net of
> a forced full frame every 4 seconds to self-heal anything missed.

> 💡 **Idea 2: a full queue is information, not an error.**
> Every stage feeds the next through a **bounded** channel (depth 3). If the
> network can't drain 60 fps at quality 85, the channel fills, `try_send`
> fails, we *drop that frame* and multiply quality/FPS by 0.75 (AIMD, like
> TCP). Latency physically cannot exceed ~3 frame intervals because there is
> nowhere for more frames to wait. Toy numbers: link chokes at t=0 → within
> 250 ms quality steps 75→56, fps 30→22 → queue drains → after ~2 s of calm,
> quality creeps back +3 every 800 ms.

> 💡 **Idea 3: discovery is advertising; trust is earned interactively.**
> Anyone on the LAN may learn "a host named 'Office PC' exists on port
> 38470" (UDP beacon). Nobody gets a single pixel without either a
> **single-use 6-digit PIN** read off the host's screen (5 attempts, 120 s
> TTL, constant-time compare) or a previously earned **256-bit token** —
> and the host stores only the token's SHA-256, so the trust store leaking
> doesn't leak credentials. Input injection is a *third*, separate grant
> the host user flips per device, re-checked on every event batch.

---

## Code

A walkthrough in dependency order.

### 1. The protocol (`crates/nebula-proto`, `viewer/web/src/protocol.ts`)

JSON control messages + one frozen binary layout. The 28-byte video header
is the wire contract:

```rust
// packet.rs — layout frozen by tests
out.push(CHANNEL_VIDEO);            // 0x01
out.push(1);                        // packet_version
out.push(self.codec as u8);         // 1=JPEG, 2=H264, 3=HEVC, 4=AV1
out.push(flags);                    // bit0 full-frame, bit1 keyframe
out.extend_from_slice(&self.frame_id.to_le_bytes());
out.extend_from_slice(&self.capture_ts_micros.to_le_bytes());
// x, y, w, h, stream_w, stream_h (u16 LE each), then payload
```

Forward compatibility is *tested*, not hoped for: unknown JSON fields and
types must parse/fail gracefully (`unknown_fields_are_ignored`,
`wire_shape_is_stable`).

### 2. The pipeline (`pipeline.rs`, `encode/`, `adaptive.rs`)

One OS thread per streaming client (capture/encode are blocking CPU work —
keeping them off the async runtime protects the server):

```rust
let Some(rect) = detector.detect(&frame.bgra, w, h) else { continue }; // Idea 1
let payload = encoder.encode_region(&frame.bgra, w, h, rect, settings.quality)?;
match tx.try_send(packet) {
    Ok(())                          => { /* stats */ }
    Err(TrySendError::Full(_))      => {              // Idea 2
        adaptive.lock().unwrap().on_send_drop();
        refresh.store(true, Ordering::Relaxed);       // resend content later
    }
    ...
}
```

Subtle bug avoided: a dropped frame's tiles are already marked "clean" in
the detector, so the drop path sets a refresh flag forcing a full frame —
otherwise that screen region would stay stale until the 4 s safety refresh.

### 3. The server state machine (`server.rs`)

One `tokio::select!` loop per connection multiplexes: incoming control
messages, pipeline packets, and a 2 s ticker (RTT pings + stats). Security
gates live here: `SessionStart` before auth → `not_authorized`; every
`Input` batch re-reads the trust store so revocation on the control panel
takes effect mid-stream (verified in the integration test).

### 4. Windows capture, input, audio (`capture/dxgi.rs`, `capture/idd.rs`, `input/windows_inject.rs`, `audio.rs`)

All real `windows`-crate code (not stubs), **type-checked in CI against
`x86_64-pc-windows-msvc`**. The DXGI source handles the classic traps:
`WAIT_TIMEOUT` (no change), `ACCESS_LOST` (mode switch → recreate),
cursor-only updates (`LastPresentTime == 0` → skip). The IddCx bridge reads
the driver's shared-memory section with a seq-before/seq-after torn-frame
check instead of cross-process locks.

### 5. The driver (`host/windows-driver/src/Driver.cpp`)

Standard IddCx choreography — `IddCxDeviceInitConfig` → `WdfDeviceCreate` →
`IddCxAdapterInitAsync` → monitor create/arrival → mode lists (720p→4K,
up to 120 Hz, portrait tablet modes) → swap-chain worker thread:

```cpp
IddCxSwapChainReleaseAndAcquireBuffer(...)   // frame from Windows
context->CopyResource(staging, texture);     // GPU → CPU
writer->WriteFrame(mapped.pData, w, h, pitch); // → shared memory + event
IddCxSwapChainFinishedProcessingFrame(...)
```

### 6. Viewers

The web viewer decodes with `createImageBitmap` and keeps **at most one**
undecoded packet ("latest wins") so a slow tablet never accumulates lag;
Android mirrors the same policy with a depth-2 `LinkedBlockingQueue`; iOS
composites via CoreGraphics (with the y-flip). All three send 1 Hz
feedback (decode ms, drops) that the host converts into FPS caps for weak
devices — quality loss is the wrong medicine when *decode* is the
bottleneck.

---

## Verification

Automated (all green in this build):

* `cargo test --workspace` — **41 tests**: protocol round-trips & frozen
  layouts, PIN lifecycle & attempt limits, token hashing/revocation,
  dirty-rect correctness incl. non-multiple-of-64 edges, JPEG validity,
  AIMD backoff/floor/recovery, pipeline full-refresh, plus a full
  **integration test** driving a real WebSocket client through
  hello → refused-before-auth → wrong PIN → pair → stream → decodable
  frames → input gating → live grant → stats → token reconnect → bad-token
  rejection.
* `cargo clippy --workspace --all-targets -- -D warnings` and
  `cargo fmt --check` — clean.
* `cargo check --target x86_64-pc-windows-msvc` — the entire Windows
  surface (DXGI/IddCx-bridge/SendInput/WASAPI/SCM service) compiles against
  the real API.
* **Browser E2E** (`tests/browser-smoke.mjs`): boots the real host binary +
  real web viewer in headless Chromium; pairs via PIN through the actual
  admin API; asserts the canvas is *animating* (pixel-samples two moments);
  toggles the stats overlay; reloads and verifies token-based reconnect
  without a PIN. Screenshots land in `tests/artifacts/`.

Manual QA script (10 minutes, Windows host):

1. `cd viewer/web && npm i && npm run build`; `cargo run -p nebula-host`.
2. Open `https://localhost:38470/` → accept cert → check Status card says
   `dxgi_desktop_duplication`.
3. Phone on the same Wi-Fi → panel "Pair a device" → scan QR → PIN
   auto-fills → your desktop appears; drag windows, watch smoothness.
4. Toggle Input for the phone in Devices → touch moves the PC cursor;
   toggle off → input stops instantly (no reconnect).
5. 📊 overlay → note rtt/fps; throttle Wi-Fi (walk away) → watch `q` drop
   and recover.
6. Driver: `installer\windows\install-driver.ps1 -TestSign` (admin,
   test-signing on) → Display Settings shows a new monitor → extend mode.

## Alternatives considered

**A. WebRTC (getDisplayMedia + RTCPeerConnection) instead of a custom pipeline**

| Pros | Cons |
|---|---|
| Hardware VP8/H.264 encode for free; congestion control (GCC) built in | No virtual-monitor path — browser capture can't extend the desktop, so the driver + native pipeline is needed anyway |
| NAT traversal machinery already exists | Latency tuning is opaque (jitter buffers you don't control); pairing/permission model still had to be built; SFU-grade complexity for a LAN tool |
| Battle-tested stacks | Rust/native WebRTC stacks are heavy; protocol wouldn't be shared with native viewers as cleanly |

Verdict: right choice for internet mode later; wrong foundation for a
LAN-first tool that owns a display driver either way.

**B. Off-the-shelf H.264 from day one (openh264/x264 or Media Foundation) instead of dirty-rect MJPEG**

| Pros | Cons |
|---|---|
| ~3–10× better compression on video-like content; keyframe/delta model matches codecs' strengths | C build deps or Windows-only MF from day one (dev/CI sandbox is Linux); patent/licensing thought needed for x264 |
| One codec end-state | Inter-frame state complicates loss/reconnect (IDR management); browsers need WebCodecs (Safari gaps) with an MJPEG fallback *anyway*; dirty-rect JPEG beats naive H.264 on static desktops |

Verdict: H.264-ready protocol + MJPEG-first shipping is the pragmatic
sequencing; the `RegionEncoder`/WebCodecs seams make the upgrade additive.

## Suggested people to talk to

This repository was created in this session, so there is no commit history
to mine for owners yet. The natural review split going forward:
whoever maintains `crates/nebula-host` networking (server.rs/pipeline.rs),
a Windows-driver reviewer for `host/windows-driver` (IddCx experience), and
a front-end owner for `viewer/web`.

## Quiz

<details>
<summary><b>Q1.</b> A viewer's Wi-Fi degrades sharply for 10 seconds. Describe the sequence of mechanisms that keep the stream usable, in order.</summary>

The WebSocket can't drain → the 3-slot pipeline channel fills → `try_send`
fails → frames are **dropped host-side** (latency stays bounded) and each
drop signals the AIMD controller → quality & FPS multiply by 0.75 (at most
once per 250 ms window) down to the profile floor → dropped content is
covered by the forced full-frame refresh flag → when congestion signals
stop for ~2 s, additive recovery (+3 quality / +4 fps every 800 ms) climbs
back. RTT inflation (>3× baseline + 40 ms) triggers the same backoff even
if the socket keeps up (bufferbloat).
</details>

<details>
<summary><b>Q2.</b> Why does the host store SHA-256 hashes of device tokens rather than the tokens, given the trust store is a local file anyway?</summary>

Defense in depth: if the trust store is exfiltrated (backup, malware with
file read, support bundle), the attacker gains no credential — hashes can't
be replayed as `Auth{token}`. The plaintext token exists only on the paired
device. It also makes accidental logging of the store harmless. (Note the
PIN is never stored at all, and comparisons are constant-time.)
</details>

<details>
<summary><b>Q3.</b> The IddCx bridge copies a frame from shared memory, then re-reads `frame_seq`. What failure does this catch, and why is it preferable to a mutex?</summary>

It catches **torn frames**: the driver overwriting the buffer mid-copy
(reader and writer are different processes at different privilege levels).
If seq changed during the copy, the frame is discarded — the next one
arrives within a frame interval, so a rare tear costs ~16 ms once. A
cross-process mutex would let a stalled *reader* block the *driver's*
swap-chain thread (priority inversion into the compositor path) and adds a
kernel object to the attack surface; seq-checking is wait-free for the
writer.
</details>

<details>
<summary><b>Q4.</b> You type one character; nothing else on screen changes. Trace what actually gets sent, from tiles to canvas.</summary>

The detector re-hashes all 64×64 tiles; exactly one tile's FNV-1a hash
differs → bounding rect = that tile (clamped at frame edges) → the encoder
repacks that rect BGRA→RGB and JPEG-encodes it (~1 KB) → a video packet
with `full_frame=0`, rect x/y/w/h, and current `stream_w/h` is sent → the
viewer decodes with `createImageBitmap` and `drawImage`s it at (x, y) onto
the persistent stream-sized canvas. If nothing at all had changed, `detect`
returns `None` and *no encode happens whatsoever*.
</details>

<details>
<summary><b>Q5.</b> Why is the admin API (PIN issuance, input grants) gated on the connection's source IP being loopback, rather than on a password?</summary>

The security boundary chosen is "person at the host machine" — the same
person who could open Display Settings. Loopback-gating makes the panel
zero-configuration (no password to lose) while keeping every LAN attacker
out (they hit 403). The documented residual risk: other processes of the
same local user can also call it — but such processes could equally read
the config directory or inject into the UI session, so a password would add
friction without adding a real boundary. Remote management would require a
proper authenticated channel, which is why it deliberately doesn't exist.
</details>
