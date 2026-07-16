// SPAKE2 over P-256 (NDSP pairing v2) — byte-compatible with
// shared/protocol/src/spake2.rs (see that file for the construction notes).
//
// Group arithmetic always uses @noble/curves: WebCrypto exposes no curve
// point operations, and the noble stack is already the audited fallback for
// everything else. HKDF/HMAC/SHA-256 also go through noble here so the PAKE
// works identically on secure and insecure origins.

import { hkdf } from "@noble/hashes/hkdf.js";
import { hmac } from "@noble/hashes/hmac.js";
import { sha256 } from "@noble/hashes/sha2.js";
import { p256 } from "@noble/curves/nist.js";

import { randomBytes } from "./caps";
import { te } from "./protocol";

const CONTEXT = te.encode("ndsp-spake2-v1");
const M_SEED = te.encode("ndsp-spake2-M-v1");
const N_SEED = te.encode("ndsp-spake2-N-v1");
const W_INFO = te.encode("ndsp-spake2-w-v1");
const ID_CLIENT = te.encode("client");
const ID_SERVER = te.encode("server");

type Point = InstanceType<typeof p256.Point>;
const ORDER: bigint = p256.Point.Fn.ORDER;

function bytesToBig(b: Uint8Array): bigint {
  let n = 0n;
  for (const x of b) n = (n << 8n) | BigInt(x);
  return n;
}

function concatBytes(...parts: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let o = 0;
  for (const p of parts) {
    out.set(p, o);
    o += p.length;
  }
  return out;
}

/** Deterministic nothing-up-my-sleeve point (must match the Rust sampler). */
function derivePoint(seed: Uint8Array): Point {
  for (let counter = 0; counter <= 255; counter++) {
    const x = sha256(concatBytes(seed, new Uint8Array([counter])));
    const candidate = new Uint8Array(33);
    candidate[0] = 0x02;
    candidate.set(x, 1);
    try {
      const p = p256.Point.fromBytes(candidate);
      p.assertValidity();
      return p;
    } catch {
      /* not on curve — try the next counter */
    }
  }
  throw new Error("unreachable: no valid point in 256 tries");
}

const M = /* @__PURE__ */ derivePoint(M_SEED);
const N = /* @__PURE__ */ derivePoint(N_SEED);

/** PIN → non-zero scalar bound to the connection nonce (matches Rust). */
function wScalar(pin: string, nonce: Uint8Array): bigint {
  for (let counter = 0; counter <= 255; counter++) {
    const digest = sha256(
      concatBytes(W_INFO, nonce, te.encode(pin), new Uint8Array([counter])),
    );
    const v = bytesToBig(digest);
    if (v > 0n && v < ORDER) return v;
  }
  throw new Error("unreachable: no valid scalar in 256 tries");
}

function randomScalar(): bigint {
  for (;;) {
    const v = bytesToBig(randomBytes(32));
    if (v > 0n && v < ORDER) return v;
  }
}

/** Length-prefixed transcript (u32-BE lengths) — must match Rust exactly. */
function transcript(
  nonce: Uint8Array,
  pa: Uint8Array,
  pb: Uint8Array,
  k: Uint8Array,
  w: bigint,
): Uint8Array {
  const wBytes = new Uint8Array(32);
  let v = w;
  for (let i = 31; i >= 0; i--) {
    wBytes[i] = Number(v & 0xffn);
    v >>= 8n;
  }
  const parts = [CONTEXT, ID_CLIENT, ID_SERVER, nonce, pa, pb, k, wBytes];
  const chunks: Uint8Array[] = [];
  for (const part of parts) {
    const len = new Uint8Array(4);
    new DataView(len.buffer).setUint32(0, part.length);
    chunks.push(len, part);
  }
  return concatBytes(...chunks);
}

export interface Spake2Keys {
  confirmClient: Uint8Array;
  confirmServer: Uint8Array;
  sessionKey: Uint8Array;
  tokenKey: Uint8Array;
}

function hkdf32(ikm: Uint8Array, info: string): Uint8Array {
  return hkdf(sha256, ikm, undefined, te.encode(info), 32);
}

function deriveKeys(tt: Uint8Array): Spake2Keys {
  const kMain = sha256(tt);
  const ka = hkdf32(kMain, "ndsp-spake2-ka-v1");
  const ke = hkdf32(kMain, "ndsp-spake2-ke-v1");
  return {
    confirmClient: hmac(sha256, hkdf32(ka, "ndsp-spake2-confirm-client-v1"), tt),
    confirmServer: hmac(sha256, hkdf32(ka, "ndsp-spake2-confirm-server-v1"), tt),
    sessionKey: hkdf32(ke, "ndsp-spake2-session-v1"),
    tokenKey: hkdf32(ke, "ndsp-spake2-token-v1"),
  };
}

/** Constant-time-ish MAC comparison (single accumulated diff). */
export function macEqual(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a[i]! ^ b[i]!;
  return diff === 0;
}

/** Client side (party A) of the exchange. */
export class Spake2Client {
  private x: bigint;
  private w: bigint;
  readonly share: Uint8Array;

  constructor(
    pin: string,
    private nonce: Uint8Array,
  ) {
    this.w = wScalar(pin, nonce);
    this.x = randomScalar();
    const pa = p256.Point.BASE.multiply(this.x).add(M.multiply(this.w));
    this.share = pa.toBytes(false); // uncompressed SEC1, 65 bytes
  }

  /** Complete with the server share pB → key schedule. Throws on bad input. */
  finish(serverShare: Uint8Array): Spake2Keys {
    const pb = p256.Point.fromBytes(serverShare);
    pb.assertValidity();
    const yPub = pb.subtract(N.multiply(this.w));
    if (yPub.equals(p256.Point.ZERO)) throw new Error("SPAKE2: degenerate server share");
    const k = yPub.multiply(this.x);
    if (k.equals(p256.Point.ZERO)) throw new Error("SPAKE2: derived identity");
    const tt = transcript(this.nonce, this.share, serverShare, k.toBytes(false), this.w);
    return deriveKeys(tt);
  }
}
