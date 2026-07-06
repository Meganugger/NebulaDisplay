// Capability detection + graceful fallbacks.
//
// The viewer is normally served by nebulad over plain HTTP on a LAN address.
// That is an *insecure context* by spec — in EVERY browser (Windows Chromium,
// iOS Safari, Android Chrome, Firefox, WebViews, …) — which removes:
//   • crypto.randomUUID          → deviceId() crashed before the handshake
//   • crypto.subtle (WebCrypto)  → the ECDH/AES handshake would crash next
//   • WebCodecs (VideoDecoder)   → H.264 decode unavailable (JPEG fallback)
// Older Safari/WebViews additionally lack PointerEvent, createImageBitmap,
// DataView#getBigUint64 and unprefixed Fullscreen. Nothing in the viewer may
// therefore assume more than ES2020 + WebSocket + canvas + getRandomValues,
// and even getRandomValues gets a last-resort escape hatch.

function detect(probe: () => boolean): boolean {
  try {
    return probe();
  } catch {
    return false;
  }
}

export const caps = {
  /** Secure context (https / localhost). Insecure ⇒ no SubtleCrypto/WebCodecs. */
  secureContext: detect(() => globalThis.isSecureContext === true),
  /** Native crypto.randomUUID (secure contexts only). */
  randomUUID: detect(() => typeof crypto.randomUUID === "function"),
  /** Native WebCrypto SubtleCrypto (secure contexts only). */
  subtle: detect(() => typeof crypto.subtle.digest === "function"),
  /** CSPRNG — present even in insecure contexts on all remotely modern engines. */
  getRandomValues: detect(() => typeof crypto.getRandomValues === "function"),
  /** WebCodecs H.264 decode path. */
  webCodecsH264: detect(
    () => typeof VideoDecoder === "function" && typeof EncodedVideoChunk === "function",
  ),
  /** Fast off-thread image decode; older iOS Safari lacks it. */
  createImageBitmap: detect(() => typeof createImageBitmap === "function"),
  /** Unified pointer events; older iOS Safari/WebViews need touch+mouse. */
  pointerEvents: detect(() => typeof PointerEvent === "function"),
  /** pointerrawupdate: input samples at device rate, not display rate. */
  pointerRawUpdate: detect(() => "onpointerrawupdate" in globalThis),
  /** Coalesced pointer samples (full touch sampling inside one event). */
  coalescedEvents: detect(
    () => typeof PointerEvent === "function" && "getCoalescedEvents" in PointerEvent.prototype,
  ),
  /** DataView 64-bit accessors (Safari ≥ 15); we fall back to u32 pairs. */
  bigInt64DataView: detect(() => typeof DataView.prototype.getBigUint64 === "function"),
  /** performance.timeOrigin (older Safari lacks it). */
  timeOrigin: detect(() => typeof performance.timeOrigin === "number"),
  /** Persistent localStorage (throws in some private modes / WebViews). */
  persistentStorage: detect(() => {
    const k = "__ndsp_probe__";
    localStorage.setItem(k, "1");
    localStorage.removeItem(k);
    return true;
  }),
};

// Engines without DataView BigInt accessors (Safari < 15) get spec-compliant
// implementations installed: our pure-JS crypto fallback (@noble/ciphers GCM)
// calls view.setBigUint64 internally, and BigInt itself is already a hard
// baseline for this codebase (ES2020 bundle target).
if (!caps.bigInt64DataView) {
  Object.defineProperty(DataView.prototype, "getBigUint64", {
    configurable: true,
    writable: true,
    value(this: DataView, offset: number, le?: boolean): bigint {
      const hi = this.getUint32(offset + (le ? 4 : 0), le);
      const lo = this.getUint32(offset + (le ? 0 : 4), le);
      return (BigInt(hi) << 32n) | BigInt(lo);
    },
  });
  Object.defineProperty(DataView.prototype, "setBigUint64", {
    configurable: true,
    writable: true,
    value(this: DataView, offset: number, value: bigint, le?: boolean): void {
      const hi = Number((value >> 32n) & 0xffffffffn);
      const lo = Number(value & 0xffffffffn);
      this.setUint32(offset + (le ? 4 : 0), hi, le);
      this.setUint32(offset + (le ? 0 : 4), lo, le);
    },
  });
}

/** One-line-per-capability diagnostics block (console + bug reports). */
export function capabilityReport(): string {
  const mark = (b: boolean) => (b ? "yes" : "no ");
  return [
    "NebulaDisplay viewer capabilities:",
    `  secure context      ${mark(caps.secureContext)}`,
    `  crypto.randomUUID   ${mark(caps.randomUUID)}${caps.randomUUID ? "" : " (RFC4122-v4 fallback)"}`,
    `  WebCrypto subtle    ${mark(caps.subtle)}${caps.subtle ? "" : " (pure-JS crypto fallback)"}`,
    `  getRandomValues     ${mark(caps.getRandomValues)}`,
    `  WebCodecs h264      ${mark(caps.webCodecsH264)}${caps.webCodecsH264 ? "" : " (JPEG streaming fallback)"}`,
    `  createImageBitmap   ${mark(caps.createImageBitmap)}${caps.createImageBitmap ? "" : " (<img> decode fallback)"}`,
    `  PointerEvent        ${mark(caps.pointerEvents)}${caps.pointerEvents ? "" : " (touch/mouse fallback)"}`,
    `  BigInt64 DataView   ${mark(caps.bigInt64DataView)}${caps.bigInt64DataView ? "" : " (u32-pair fallback)"}`,
    `  fullscreen API      ${mark(fullscreen.supported)}`,
    `  persistent storage  ${mark(caps.persistentStorage)}${caps.persistentStorage ? "" : " (in-memory; pairing lost on reload)"}`,
  ].join("\n");
}

// --- WebCodecs H.264 decode probe --------------------------------------------

let h264Probe: Promise<boolean> | null = null;

/**
 * True only when the engine can actually decode our H.264 stream. API
 * existence is NOT enough: Chromium/Electron builds without proprietary
 * codecs expose VideoDecoder yet reject avc1 configs — advertising h264
 * there would produce a black canvas. Memoized.
 */
export function probeH264Decode(): Promise<boolean> {
  if (!caps.webCodecsH264) return Promise.resolve(false);
  if (!h264Probe) {
    h264Probe = (async () => {
      try {
        if (typeof VideoDecoder.isConfigSupported !== "function") return true; // pre-probe engines
        const res = await VideoDecoder.isConfigSupported({
          codec: "avc1.42E01F",
          optimizeForLatency: true,
        });
        return res.supported === true;
      } catch {
        return false;
      }
    })();
  }
  return h264Probe;
}

// --- randomness + UUID ------------------------------------------------------

let warnedInsecureRandom = false;

/** CSPRNG bytes; last-resort Math.random path never hard-crashes the viewer. */
export function randomBytes(n: number): Uint8Array {
  const out = new Uint8Array(n);
  if (caps.getRandomValues) {
    crypto.getRandomValues(out);
    return out;
  }
  if (!warnedInsecureRandom) {
    warnedInsecureRandom = true;
    console.warn(
      "crypto.getRandomValues is unavailable — falling back to Math.random. " +
        "Randomness is NOT cryptographically secure in this environment.",
    );
  }
  for (let i = 0; i < n; i++) out[i] = Math.floor(Math.random() * 256);
  return out;
}

const HEX: string[] = [];
for (let i = 0; i < 256; i++) HEX.push((i + 0x100).toString(16).slice(1));

/**
 * UUID v4. Uses crypto.randomUUID when available (secure contexts), otherwise
 * generates RFC4122 v4 from crypto.getRandomValues. Works on Chromium,
 * Firefox, Safari, iOS Safari, Android Chrome, WebViews and Electron.
 */
export function generateUuid(): string {
  if (caps.randomUUID) {
    try {
      return crypto.randomUUID();
    } catch {
      // e.g. detached/odd realms — fall through to the manual path.
    }
  }
  const b = randomBytes(16);
  b[6] = (b[6]! & 0x0f) | 0x40; // version 4
  b[8] = (b[8]! & 0x3f) | 0x80; // RFC4122 variant 10xx
  return (
    HEX[b[0]!]! + HEX[b[1]!]! + HEX[b[2]!]! + HEX[b[3]!]! +
    "-" + HEX[b[4]!]! + HEX[b[5]!]! +
    "-" + HEX[b[6]!]! + HEX[b[7]!]! +
    "-" + HEX[b[8]!]! + HEX[b[9]!]! +
    "-" + HEX[b[10]!]! + HEX[b[11]!]! + HEX[b[12]!]! + HEX[b[13]!]! + HEX[b[14]!]! + HEX[b[15]!]!
  );
}

// --- storage ----------------------------------------------------------------

const memoryStore = new Map<string, string>();

/**
 * localStorage with an in-memory fallback: Safari private mode and some
 * WebViews throw on any localStorage access, and quota errors can appear
 * mid-session. Never throws.
 */
export const storage = {
  persistent: caps.persistentStorage,
  get(key: string): string | null {
    if (caps.persistentStorage) {
      try {
        return localStorage.getItem(key);
      } catch {
        /* fall through */
      }
    }
    return memoryStore.get(key) ?? null;
  },
  set(key: string, value: string): void {
    memoryStore.set(key, value);
    if (caps.persistentStorage) {
      try {
        localStorage.setItem(key, value);
      } catch {
        /* quota / private mode flipped mid-session — memory copy still works */
      }
    }
  },
  remove(key: string): void {
    memoryStore.delete(key);
    if (caps.persistentStorage) {
      try {
        localStorage.removeItem(key);
      } catch {
        /* ignore */
      }
    }
  },
};

// --- fullscreen (webkit-prefixed on iOS/older Safari) -------------------------

interface FullscreenDoc extends Document {
  webkitFullscreenElement?: Element | null;
  webkitFullscreenEnabled?: boolean;
  webkitExitFullscreen?: () => Promise<void> | void;
}
interface FullscreenEl extends HTMLElement {
  webkitRequestFullscreen?: () => Promise<void> | void;
}

export const fullscreen = {
  get supported(): boolean {
    const d = document as FullscreenDoc;
    return detect(
      () =>
        typeof HTMLElement.prototype.requestFullscreen === "function" ||
        typeof (HTMLElement.prototype as FullscreenEl).webkitRequestFullscreen === "function",
    ) && (d.fullscreenEnabled ?? d.webkitFullscreenEnabled ?? true);
  },
  element(): Element | null {
    const d = document as FullscreenDoc;
    return d.fullscreenElement ?? d.webkitFullscreenElement ?? null;
  },
  async enter(el: HTMLElement): Promise<void> {
    const e = el as FullscreenEl;
    try {
      if (typeof e.requestFullscreen === "function") await e.requestFullscreen();
      else if (typeof e.webkitRequestFullscreen === "function") await e.webkitRequestFullscreen();
    } catch {
      /* user-gesture / platform denial — non-fatal */
    }
  },
  async exit(): Promise<void> {
    const d = document as FullscreenDoc;
    try {
      if (typeof d.exitFullscreen === "function") await d.exitFullscreen();
      else if (typeof d.webkitExitFullscreen === "function") await d.webkitExitFullscreen();
    } catch {
      /* ignore */
    }
  },
};

// --- 64-bit DataView access (Safari < 15 lacks BigInt accessors) --------------

export function readU64BE(dv: DataView, offset: number): bigint {
  if (caps.bigInt64DataView) return dv.getBigUint64(offset);
  return (BigInt(dv.getUint32(offset)) << 32n) | BigInt(dv.getUint32(offset + 4));
}

export function writeU64BE(dv: DataView, offset: number, value: bigint): void {
  if (caps.bigInt64DataView) {
    dv.setBigUint64(offset, value);
    return;
  }
  dv.setUint32(offset, Number((value >> 32n) & 0xffffffffn));
  dv.setUint32(offset + 4, Number(value & 0xffffffffn));
}

/** Epoch-anchored now() in ms even where performance.timeOrigin is missing. */
const epochBaseMs: number = caps.timeOrigin
  ? performance.timeOrigin
  : Date.now() - performance.now();

export function epochNowMs(): number {
  return epochBaseMs + performance.now();
}
