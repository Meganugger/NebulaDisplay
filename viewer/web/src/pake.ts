// SPAKE2 over P-256 — byte-compatible with shared/protocol/src/pake.rs.
//
// PAKE math needs elliptic-curve *point arithmetic*, which WebCrypto does not
// expose, so this module always uses the audited pure-JS @noble libraries
// (already bundled for the insecure-context fallback). Only the pairing
// handshake runs through here — envelope crypto still uses the native
// WebCrypto backend where available (see ./cryptobox).
//
// Protocol notes (must mirror the Rust side exactly):
// * group elements M and N are derived by deterministic try-and-increment
//   hash-to-curve from public seed strings (nothing up my sleeve);
// * w = SHA-256("ndsp-pake-w-v1" ‖ u64le(len(nonce)) ‖ nonce ‖ PIN) mod n;
// * transcript TT length-prefixes (u64 LE) idA, idB, M, N, pA, pB, K, w;
// * Ka = SHA-256(TT); confirmation keys + session key via HKDF(Ka, nonce, …);
// * cA/cB = HMAC-SHA256(KcA/KcB, TT); the CLIENT confirms first so the host
//   can rate-limit wrong-PIN attempts.

import { p256 } from "@noble/curves/nist.js";
import { hkdf } from "@noble/hashes/hkdf.js";
import { hmac } from "@noble/hashes/hmac.js";
import { sha256 } from "@noble/hashes/sha2.js";

import { randomBytes } from "./caps";
import { te } from "./protocol";

const Point = p256.Point;
type P256Point = InstanceType<typeof Point>;
const ORDER: bigint = Point.Fn.ORDER;

const H2C_TAG = te.encode("ndsp-h2c-p256-v1");
const M_SEED = te.encode("NebulaDisplay SPAKE2 point M v1");
const N_SEED = te.encode("NebulaDisplay SPAKE2 point N v1");
const W_INFO = te.encode("ndsp-pake-w-v1");
const ID_CLIENT = te.encode("ndsp-pake-client");
const ID_SERVER = te.encode("ndsp-pake-server");
const CONFIRM_A_INFO = te.encode("ndsp-pake-confirm-a");
const CONFIRM_B_INFO = te.encode("ndsp-pake-confirm-b");
const SESSION_INFO = te.encode("ndsp-session-v1");

function concat(...parts: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let o = 0;
  for (const p of parts) {
    out.set(p, o);
    o += p.length;
  }
  return out;
}

function u64le(n: number): Uint8Array {
  const b = new Uint8Array(8);
  new DataView(b.buffer).setBigUint64(0, BigInt(n), true);
  return b;
}

function u32be(n: number): Uint8Array {
  const b = new Uint8Array(4);
  new DataView(b.buffer).setUint32(0, n, false);
  return b;
}

function bytesToBigInt(bytes: Uint8Array): bigint {
  let hex = "";
  for (const b of bytes) hex += b.toString(16).padStart(2, "0");
  return hex.length ? BigInt("0x" + hex) : 0n;
}

function bigIntTo32(n: bigint): Uint8Array {
  const hex = n.toString(16).padStart(64, "0");
  const out = new Uint8Array(32);
  for (let i = 0; i < 32; i++) out[i] = parseInt(hex.slice(i * 2, i * 2 + 2), 16);
  return out;
}

/** Deterministic try-and-increment hash-to-curve (even-y point). */
function hashToCurve(seed: Uint8Array): P256Point {
  for (let ctr = 0; ctr < 0xffffffff; ctr++) {
    const x = sha256(concat(H2C_TAG, u64le(seed.length), seed, u32be(ctr)));
    const enc = new Uint8Array(33);
    enc[0] = 0x02;
    enc.set(x, 1);
    try {
      return Point.fromBytes(enc);
    } catch {
      /* x not a valid coordinate — try the next counter */
    }
  }
  throw new Error("hash-to-curve failed (unreachable)");
}

let mCache: P256Point | null = null;
let nCache: P256Point | null = null;
function mPoint(): P256Point {
  return (mCache ??= hashToCurve(M_SEED));
}
function nPoint(): P256Point {
  return (nCache ??= hashToCurve(N_SEED));
}

/** SPAKE2 password scalar w (nonzero mod n). */
function passwordScalar(pin: string, nonce: Uint8Array): bigint {
  const digest = sha256(concat(W_INFO, u64le(nonce.length), nonce, te.encode(pin)));
  return bytesToBigInt(digest) % ORDER;
}

function randomScalar(): bigint {
  for (;;) {
    const k = bytesToBigInt(randomBytes(32)) % ORDER;
    if (k !== 0n) return k;
  }
}

/** k·P handling k = 0 (returns identity) — noble's multiply rejects 0. */
function mul(p: P256Point, k: bigint): P256Point {
  return k === 0n ? Point.ZERO : p.multiply(k);
}

function pushLv(parts: Uint8Array[], part: Uint8Array): void {
  parts.push(u64le(part.length), part);
}

function transcript(pa: P256Point, pb: P256Point, k: P256Point, w: bigint): Uint8Array {
  const parts: Uint8Array[] = [];
  pushLv(parts, ID_CLIENT);
  pushLv(parts, ID_SERVER);
  pushLv(parts, mPoint().toBytes(false));
  pushLv(parts, nPoint().toBytes(false));
  pushLv(parts, pa.toBytes(false));
  pushLv(parts, pb.toBytes(false));
  pushLv(parts, k.toBytes(false));
  pushLv(parts, bigIntTo32(w));
  return concat(...parts);
}

function ctEq(a: Uint8Array, b: Uint8Array): boolean {
  if (a.length !== b.length) return false;
  let diff = 0;
  for (let i = 0; i < a.length; i++) diff |= a[i]! ^ b[i]!;
  return diff === 0;
}

export interface PakePending {
  /** Client confirmation MAC cA — send this immediately. */
  confirmA: Uint8Array;
  /** Verify the server's cB; only then is the session key released. */
  verifyServer(serverConfirm: Uint8Array): Uint8Array;
}

export interface PakeClientState {
  /** pA to send in `pake_start` (uncompressed SEC1, 65 bytes). */
  shareBytes: Uint8Array;
  /** Process the server's pB from `pake_response`. */
  finish(serverShare: Uint8Array): PakePending;
}

/** Begin SPAKE2 pairing: pA = x·G + w·M. */
export function pakeClientStart(pin: string, nonce: Uint8Array): PakeClientState {
  const w = passwordScalar(pin, nonce);
  const x = randomScalar();
  const pa = Point.BASE.multiply(x).add(mul(mPoint(), w));
  return {
    shareBytes: pa.toBytes(false),
    finish(serverShare: Uint8Array): PakePending {
      const pb = Point.fromBytes(serverShare); // throws on invalid/off-curve
      if (pb.is0()) throw new Error("PAKE share is the identity point");
      // K = x·(pB − w·N)
      const k = pb.subtract(mul(nPoint(), w)).multiply(x);
      if (k.is0()) throw new Error("degenerate PAKE shared point");
      const tt = transcript(pa, pb, k, w);
      const ka = sha256(tt);
      const kcA = hkdf(sha256, ka, nonce, CONFIRM_A_INFO, 32);
      const kcB = hkdf(sha256, ka, nonce, CONFIRM_B_INFO, 32);
      const sessionKey = hkdf(sha256, ka, nonce, SESSION_INFO, 32);
      const confirmBExpected = hmac(sha256, kcB, tt);
      return {
        confirmA: hmac(sha256, kcA, tt),
        verifyServer(serverConfirm: Uint8Array): Uint8Array {
          if (!ctEq(confirmBExpected, serverConfirm)) {
            throw new Error("PAKE server confirmation failed (wrong PIN or tampering)");
          }
          return sessionKey;
        },
      };
    },
  };
}
