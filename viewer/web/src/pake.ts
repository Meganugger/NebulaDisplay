// SPAKE2 PAKE for pairing — byte-compatible with shared/protocol/src/pake.rs
// (locked by tests/pake-vectors.mjs against the Rust fixed vector).
//
// Runs on @noble/curves point arithmetic in *every* context: WebCrypto has no
// EC point API, and pairing happens once per device, so the audited pure-JS
// path is both necessary and cheap here. Session/envelope crypto still uses
// native WebCrypto where available (see cryptobox.ts).
//
// Construction (see docs/PROTOCOL.md §PAKE pairing):
//   w  = OS2IP(SHA-256("ndsp-pake-w-v1" ‖ lp(pin) ‖ lp(salt) ‖ lp(nonce))) mod n   (0 → 1)
//   pA = x·G + w·M   (client)          pB = y·G + w·N   (server)
//   Z  = x·(pB − w·N)
//   TT = SHA-256("ndsp-pake-v1" ‖ lp(nonce) ‖ lp(salt) ‖ lp(clientEcdhPub)
//                ‖ lp(serverEcdhPub) ‖ lp(pA) ‖ lp(pB) ‖ lp(Z) ‖ lp(w))
//   pairKey = HKDF-SHA256(ikm = TT, salt, info = "ndsp-pair-pake-v1" ‖ nonce)
// lp() = u16-BE length prefix; points are uncompressed SEC1 (65 bytes).

import { p256 } from "@noble/curves/nist.js";
import { hkdf } from "@noble/hashes/hkdf.js";
import { sha256 } from "@noble/hashes/sha2.js";

import { randomBytes } from "./caps";
import { te } from "./protocol";

const Point = p256.Point;
const ORDER: bigint = Point.Fn.ORDER;

// RFC 9382 §6 fixed points for P-256 (compressed SEC1).
const M = Point.fromHex("02886e2f97ace46e55ba9dd7242579f2993b64e16ef3dcab95afd497333d8fa12f");
const N = Point.fromHex("03d8bbd6c639c62937b04d997f38c3770719c629d7014d49a24b4f98baa1292b49");

const W_INFO = te.encode("ndsp-pake-w-v1");
const TT_CONTEXT = te.encode("ndsp-pake-v1");
const KEY_INFO = te.encode("ndsp-pair-pake-v1");

function bytesToBig(b: Uint8Array): bigint {
  let n = 0n;
  for (const x of b) n = (n << 8n) | BigInt(x);
  return n;
}

function bigTo32(n: bigint): Uint8Array {
  const out = new Uint8Array(32);
  for (let i = 31; i >= 0; i--) {
    out[i] = Number(n & 0xffn);
    n >>= 8n;
  }
  return out;
}

/** u16-BE length-prefixed transcript item (must match the Rust `lp`). */
function lp(parts: Uint8Array[], data: Uint8Array): void {
  if (data.length > 0xffff) throw new Error("transcript item too long");
  parts.push(new Uint8Array([data.length >> 8, data.length & 0xff]), data);
}

function concat(parts: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let o = 0;
  for (const p of parts) {
    out.set(p, o);
    o += p.length;
  }
  return out;
}

function deriveW(pin: string, salt: Uint8Array, nonce: Uint8Array): bigint {
  const parts: Uint8Array[] = [W_INFO];
  lp(parts, te.encode(pin));
  lp(parts, salt);
  lp(parts, nonce);
  const w = bytesToBig(sha256(concat(parts))) % ORDER;
  return w === 0n ? 1n : w;
}

export interface PakeClient {
  /** Our share pA, uncompressed SEC1 (65 bytes). */
  readonly share: Uint8Array;
  /** Complete with the server share pB + both ECDH handshake pubkeys. */
  finish(serverShare: Uint8Array, clientEcdhPub: Uint8Array, serverEcdhPub: Uint8Array): Uint8Array;
}

/** Random scalar in [1, n-1]. */
function randomScalar(): bigint {
  // Rejection sampling over 32 random bytes (bias-free, terminates fast).
  for (;;) {
    const n = bytesToBig(randomBytes(32));
    if (n >= 1n && n < ORDER) return n;
  }
}

export function startPake(
  pin: string,
  salt: Uint8Array,
  nonce: Uint8Array,
  testSecret?: bigint,
): PakeClient {
  const w = deriveW(pin, salt, nonce);
  const x = testSecret ?? randomScalar();
  const shareP = Point.BASE.multiply(x).add(M.multiply(w));
  const share = shareP.toBytes(false);
  return {
    share,
    finish(serverShare, clientEcdhPub, serverEcdhPub): Uint8Array {
      const pB = Point.fromBytes(serverShare); // validates on-curve
      const z = pB.subtract(N.multiply(w)).multiply(x);
      if (z.equals(Point.ZERO)) throw new Error("degenerate PAKE shared point");
      const parts: Uint8Array[] = [TT_CONTEXT];
      lp(parts, nonce);
      lp(parts, salt);
      lp(parts, clientEcdhPub);
      lp(parts, serverEcdhPub);
      lp(parts, share);
      lp(parts, serverShare);
      lp(parts, z.toBytes(false));
      lp(parts, bigTo32(w));
      const tt = sha256(concat(parts));
      return hkdf(sha256, tt, salt, concat([KEY_INFO as Uint8Array, nonce]), 32);
    },
  };
}
