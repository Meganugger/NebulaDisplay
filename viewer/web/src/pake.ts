// Balanced PAKE for PIN pairing (CPace-style, ristretto255).
// Byte-compatible with shared/protocol/src/pake.rs — cross-checked by
// tests/pake-vector.mjs against the Rust test vector, and end-to-end by
// tests/web-compat.mjs pairing against the real host.
//
// WebCrypto has no ristretto255, so this always uses @noble/curves (already
// bundled for the insecure-context fallback backend). The PIN-bound
// generator makes a recorded transcript useless for offline PIN grinding:
// testing a candidate PIN requires solving CDH in the group.

import { ristretto255, ristretto255_hasher } from "@noble/curves/ed25519.js";
import { sha512 } from "@noble/hashes/sha2.js";

import { randomBytes } from "./caps";
import { te } from "./protocol";

const DSI = te.encode("ndsp-pake-v1");

type RPoint = InstanceType<typeof ristretto255.Point>;

// deriveToCurve (RFC 9496 §4.3.4 element derivation) is always defined for
// ristretto255 in @noble/curves; the base hasher type just marks it optional.
const deriveToCurveMaybe = ristretto255_hasher.deriveToCurve;
if (!deriveToCurveMaybe) throw new Error("@noble/curves ristretto255 lacks deriveToCurve");
const deriveToCurve: NonNullable<typeof deriveToCurveMaybe> = deriveToCurveMaybe;

function concat(...parts: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(parts.reduce((n, p) => n + p.length, 0));
  let o = 0;
  for (const p of parts) {
    out.set(p, o);
    o += p.length;
  }
  return out;
}

/** PIN-bound generator: map_to_group(SHA-512(DSI ‖ len(pin) ‖ pin ‖ nonce)). */
function generator(pin: string, connectionNonce: Uint8Array): RPoint {
  const pinBytes = te.encode(pin);
  if (pinBytes.length > 255) throw new Error("PIN too long");
  const uniform = sha512(concat(DSI, new Uint8Array([pinBytes.length]), pinBytes, connectionNonce));
  return deriveToCurve(uniform);
}

/** Uniform nonzero scalar from 64 random bytes (little-endian, mod group order). */
function randomScalar(): bigint {
  const order = ristretto255.Point.Fn.ORDER;
  for (;;) {
    const wide = randomBytes(64);
    let n = 0n;
    for (let i = wide.length - 1; i >= 0; i--) n = (n << 8n) | BigInt(wide[i]!);
    const s = n % order;
    if (s !== 0n) return s;
  }
}

export interface PakeExchange {
  /** Our public share (canonical ristretto255 encoding, 32 bytes). */
  share: Uint8Array;
  /** Combine with the peer's share → 32-byte shared secret. */
  finish(peerShare: Uint8Array): Uint8Array;
}

export function pakeStart(pin: string, connectionNonce: Uint8Array): PakeExchange {
  const g = generator(pin, connectionNonce);
  const scalar = randomScalar();
  const share = g.multiply(scalar).toBytes();
  return {
    share,
    finish(peerShare: Uint8Array): Uint8Array {
      if (peerShare.length !== 32) throw new Error("PAKE share must be 32 bytes");
      // fromBytes validates canonical encoding; ZERO is the identity.
      const peer = ristretto255.Point.fromBytes(peerShare);
      if (peer.equals(ristretto255.Point.ZERO)) throw new Error("identity PAKE share rejected");
      const k = peer.multiply(scalar);
      if (k.equals(ristretto255.Point.ZERO)) throw new Error("degenerate PAKE result");
      return k.toBytes();
    },
  };
}
