# Web viewer browser compatibility

## The insecure-context reality

nebulad serves the web viewer over **plain HTTP on a LAN address** (zero-install,
no certificates). Browsers treat every non-`localhost` HTTP origin as an
**insecure context**, which removes several modern APIs *by spec* — on all
engines, including current Windows Chromium, iOS Safari and Android Chrome:

| API | In insecure contexts | Viewer behavior |
|---|---|---|
| `crypto.randomUUID` | **absent** | RFC4122-v4 generator from `crypto.getRandomValues` (`src/caps.ts`) |
| `crypto.subtle` (WebCrypto) | **absent** | audited pure-JS backend: `@noble/curves` (ECDH P-256), `@noble/hashes` (HKDF/SHA-256), `@noble/ciphers` (AES-256-GCM) — byte-compatible with the Rust host (`src/cryptobox.ts`) |
| WebCodecs (`VideoDecoder`) | **absent** | **H.264 via MSE**: Media Source Extensions *are* available on insecure origins, so the viewer remuxes the host's Annex-B H.264 into single-frame fMP4 fragments client-side (`src/fmp4.ts`) and plays them through a hidden `<video>` + `requestVideoFrameCallback` → canvas (`MseSink` in `src/decoder.ts`). JPEG remains the final fallback for MSE-less engines. |

This is why the viewer historically "crashed before the pairing handshake" on
real devices while passing `localhost` tests: `localhost` is a secure context.

Decoder selection order for H.264: WebCodecs (secure contexts, lowest
latency, real per-frame decode timing) → MSE/fMP4 (insecure contexts; one
frame per fragment + live-edge chasing keeps added latency to roughly one
frame) → JPEG (universal). Negotiated per session in the `hello`; verified
end-to-end by compat-E2E scenario 2, which streams real H.264 on an insecure
LAN origin.

The only paths to native crypto/WebCodecs over LAN are HTTPS with a
user-trusted certificate or a packaged (native/Electron) viewer — both remain
on the roadmap; the fallback layer keeps the zero-install promise working today.

## Capability layer

`src/caps.ts` centralizes all feature detection; nothing else in the viewer may
touch a modern API without going through it:

| Capability | Fallback |
|---|---|
| `crypto.randomUUID` | RFC4122 v4 via `getRandomValues` (last resort: `Math.random`, flagged insecure, never hard-crashes) |
| `crypto.subtle` | pure-JS backend selected once in `src/cryptobox.ts` |
| WebCodecs H.264 | **probed with `VideoDecoder.isConfigSupported`** — API existence is not enough: codec-less Chromium/Electron builds expose `VideoDecoder` but reject `avc1` configs. JPEG otherwise. Repeated decoder errors surface a visible error instead of a black canvas. |
| `createImageBitmap` | JPEG decode through an `<img>` element (older iOS Safari) |
| `PointerEvent` | raw `touch*` + `mouse*` listeners (older iOS Safari / WebViews) |
| `pointerrawupdate` | `pointermove` (display-rate instead of device-rate move sampling) |
| `getCoalescedEvents` | the single event's coordinates (fewer samples per move) |
| `localStorage` | in-memory store (Safari private mode / locked-down WebViews); UI notes that pairing won't persist |
| Fullscreen API | `webkit`-prefixed variants; button hidden when absent (iPhone Safari) |
| `DataView#getBigUint64/setBigUint64` | spec-compliant implementation installed when missing (Safari < 15; also required by `@noble/ciphers`) |
| `performance.timeOrigin` | `Date.now() - performance.now()` epoch base |
| `wss://` vs `ws://` | matches the page scheme so an HTTPS deployment works unchanged |

Every page load prints a capability report to the console
(`capabilityReport()`), and the connect card shows a "Compatibility mode" note
when the crypto or storage fallback is active.

## Verified environments (automated, `tests/compat-e2e.mjs`)

Real Chromium against a real host in six scenarios, all required to pair,
finish the encrypted handshake and render moving video:

1. secure context (`localhost`) — native WebCrypto + real-capability codec choice (regression guard);
2. insecure LAN origin — the exact Windows-Chromium/iOS-Safari crash environment: fallback crypto, JPEG, token reconnect after reload;
3. iOS-Safari-like — insecure + no `PointerEvent`/`createImageBitmap`/DataView BigInt/`timeOrigin`/fullscreen, touch input, iPhone UA/viewport;
4. Android-Chrome-like — insecure + touch, Android UA;
5. storage-blocked WebView — `localStorage` throws;
6. regression guard for the original `crypto.randomUUID` crash.

`tests/web-compat.mjs` additionally proves **both** crypto backends
byte-compatible with the Rust host (run twice: native, and with
`NDSP_CRYPTO=fallback`).

Emulation approximates real devices. The manual release-gate matrix in
`docs/TESTING.md` still requires a real iPhone/iPad Safari and a real Android
Chrome pass, since engine-specific decode/scheduling behavior can't be fully
emulated from Chromium.
