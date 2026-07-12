// SPAKE2 over P-256 for PIN pairing (auth method "pair_pake").
// Byte-compatible with shared/protocol/src/crypto.rs `crypto::pake` — the
// cross-stack handshake is verified against the real Rust host by
// tests/web-compat.mjs on both crypto backends.
//
// Point arithmetic always uses @noble/curves (WebCrypto has no EC group
// API); the HKDF steps go through ./cryptobox, which picks native WebCrypto
// where available.
//
// The blinding points M/N are the standard SPAKE2 constants for P-256 from
// RFC 9382 §6 (public documentation, nothing-up-my-sleeve seeded points).

import { p256 } from "@noble/curves/nist.js";

import { randomBytes } from "./caps";
import { hkdfSha256 } from "./cryptobox";
import { te } from "./protocol";

const W_INFO = te.encode("ndsp-pake-w-v1");
const PAIR_INFO = te.encode("ndsp-pake-pair-v1");
const SESSION_INFO = te.encode("ndsp-pake-session-v1");

const M = p256.Point.fromHex("02886e2f97ace46e55ba9dd7242579f2993b64e16ef3dcab95afd497333d8fa12f");
const N = p256.Point.fromHex("03d8bbd6c639c62937b04d997f38c3770719c629d7014d49a24b4f98baa1292b49");

function bytesToBigInt(b: Uint8Array): bigint {
  let n = 0n;
  for (const x of b) n = (n << 8n) | BigInt(x);
  return n;
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

/** Uniform scalar in [1, n-1] via rejection sampling (bias-free). */
function randomScalar(): bigint {
  const order = p256.Point.Fn.ORDER;
  for (;;) {
    const k = bytesToBigInt(randomBytes(32));
    if (k >= 1n && k < order) return k;
  }
}

export interface PakeKeys {
  pairKey: Uint8Array;
  sessionKey: Uint8Array;
}

/** Client side of one SPAKE2 exchange. */
export class PakeClient {
  private constructor(
    private x: bigint,
    private w: bigint,
    private wBytes: Uint8Array,
    /** X = x·G + w·M, uncompressed SEC1 (65 bytes) — goes in `pair_start`. */
    readonly publicShare: Uint8Array,
  ) {}

  /** Derive the password scalar and produce this side's blinded share. */
  static async start(pin: string, connectionNonce: Uint8Array): Promise<PakeClient> {
    const wBytes = await hkdfSha256(te.encode(pin), connectionNonce, W_INFO);
    let w = bytesToBigInt(wBytes) % p256.Point.Fn.ORDER;
    if (w === 0n) w = 1n; // matches the Rust side's zero-avoidance
    const x = randomScalar();
    const share = p256.Point.BASE.multiply(x).add(M.multiply(w));
    return new PakeClient(x, w, wBytes, share.toBytes(false));
  }

  /**
   * Complete with the server's `pair_challenge` share, deriving both keys.
   * Throws on off-curve/identity shares or a degenerate shared point.
   */
  async complete(
    serverShare: Uint8Array,
    salt: Uint8Array,
    connectionNonce: Uint8Array,
  ): Promise<PakeKeys> {
    const y = p256.Point.fromBytes(serverShare); // validates on-curve, non-zero
    const z = y.subtract(N.multiply(this.w)).multiply(this.x);
    if (z.is0()) throw new Error("degenerate PAKE shared point");
    const zBytes = z.toBytes(false);
    const tail = concat(connectionNonce, this.publicShare, serverShare, this.wBytes);
    return {
      pairKey: await hkdfSha256(zBytes, salt, concat(PAIR_INFO, tail)),
      sessionKey: await hkdfSha256(zBytes, salt, concat(SESSION_INFO, tail)),
    };
  }
}
