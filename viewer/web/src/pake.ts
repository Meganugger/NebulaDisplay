// SPAKE2 (RFC 9382) over P-256 — the client (prover) side of NDSP's PAKE
// pairing. Byte-compatible with shared/protocol/src/pake.rs (verified by
// tests/web-compat.mjs against the real Rust host on both crypto backends).
//
// Point arithmetic always uses @noble/curves: WebCrypto exposes no group
// operations, and noble is already bundled for the insecure-context fallback.
// Hashing/HKDF go through ./cryptobox so they use native WebCrypto when
// available.
//
//   w  = HKDF-SHA256(ikm=PIN, salt=connection_nonce, info="ndsp-spake2-w-v1") mod n
//   pA = x·G + w·M                     (→ PairStart.client_pubkey)
//   K  = x·(pB − w·N)                  (pB ← PairChallenge.server_pubkey)
//   TT = ‖ len₈LE(part) || part  for ["ndsp-spake2-v1","client","server",
//                                     nonce, pA, pB, K, w]
//   pairing_key = HKDF(SHA-256(TT), salt, "ndsp-spake2-pair-v1")
//   session_key = HKDF(SHA-256(TT), salt, "ndsp-spake2-session-v1" || nonce)

import { p256 } from "@noble/curves/nist.js";

import { randomBytes } from "./caps";
import { hkdfSha256, sha256 } from "./cryptobox";
import { te } from "./protocol";

const Point = p256.Point;
const ORDER: bigint = Point.Fn.ORDER;

// RFC 9382 §4 nothing-up-my-sleeve points for P-256.
const M = Point.fromHex("02886e2f97ace46e55ba9dd7242579f2993b64e16ef3dcab95afd497333d8fa12f");
const N = Point.fromHex("03d8bbd6c639c62937b04d997f38c3770719c629d7014d49a24b4f98baa1292b49");

const W_INFO = te.encode("ndsp-spake2-w-v1");
const CONTEXT = te.encode("ndsp-spake2-v1");
const PAIR_INFO = te.encode("ndsp-spake2-pair-v1");
const SESSION_INFO = te.encode("ndsp-spake2-session-v1");

function bytesToBig(b: Uint8Array): bigint {
  let n = 0n;
  for (const x of b) n = (n << 8n) | BigInt(x);
  return n;
}

/** Big-endian 32-byte encoding (matches Rust `Scalar::to_bytes`). */
function bigTo32(n: bigint): Uint8Array {
  const out = new Uint8Array(32);
  for (let i = 31; i >= 0; i--) {
    out[i] = Number(n & 0xffn);
    n >>= 8n;
  }
  return out;
}

function nonZeroMod(n: bigint): bigint {
  const r = n % ORDER;
  return r === 0n ? 1n : r;
}

async function deriveW(pin: string, connectionNonce: Uint8Array): Promise<bigint> {
  const okm = await hkdfSha256(te.encode(pin), connectionNonce, W_INFO);
  return nonZeroMod(bytesToBig(okm));
}

export interface PakeKeys {
  pairingKey: Uint8Array;
  sessionKey: Uint8Array;
}

function concat(...parts: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let o = 0;
  for (const p of parts) {
    out.set(p, o);
    o += p.length;
  }
  return out;
}

/** `‖ len(part) as u64 LE || part` — must match the Rust transcript. */
function transcript(parts: Uint8Array[]): Uint8Array {
  let total = 0;
  for (const p of parts) total += 8 + p.length;
  const out = new Uint8Array(total);
  const dv = new DataView(out.buffer);
  let o = 0;
  for (const p of parts) {
    dv.setUint32(o, p.length, true); // lengths are far below 2^32
    o += 8;
    out.set(p, o);
    o += p.length;
  }
  return out;
}

export class Spake2Client {
  private constructor(
    private readonly x: bigint,
    private readonly w: bigint,
    /** pA, uncompressed SEC1 (65 bytes) — send as `PairStart.client_pubkey`. */
    public readonly publicRaw: Uint8Array,
  ) {}

  static async start(pin: string, connectionNonce: Uint8Array): Promise<Spake2Client> {
    const w = await deriveW(pin, connectionNonce);
    const x = nonZeroMod(bytesToBig(randomBytes(32)));
    const pa = Point.BASE.multiply(x).add(M.multiply(w));
    return new Spake2Client(x, w, pa.toBytes(false));
  }

  /** Complete with the server's pB (from `PairChallenge`) → derived keys. */
  async finish(
    serverElement: Uint8Array,
    connectionNonce: Uint8Array,
    salt: Uint8Array,
  ): Promise<PakeKeys> {
    const pb = Point.fromBytes(serverElement); // throws off-curve/malformed
    if (pb.is0()) throw new Error("SPAKE2: server element is the identity");
    const unblinded = pb.subtract(N.multiply(this.w));
    if (unblinded.is0()) throw new Error("SPAKE2: degenerate server element");
    const k = unblinded.multiply(this.x);
    if (k.is0()) throw new Error("SPAKE2: shared element is the identity");

    const tt = transcript([
      CONTEXT,
      te.encode("client"),
      te.encode("server"),
      connectionNonce,
      this.publicRaw,
      serverElement,
      k.toBytes(false),
      bigTo32(this.w),
    ]);
    const h = await sha256(tt);
    return {
      pairingKey: await hkdfSha256(h, salt, PAIR_INFO),
      sessionKey: await hkdfSha256(h, salt, concat(SESSION_INFO, connectionNonce)),
    };
  }
}
