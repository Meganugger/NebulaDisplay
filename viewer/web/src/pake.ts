// NDSP-PAKE v1 client — CPace-style balanced PAKE on P-256.
// Byte-compatible with shared/protocol/src/pake.rs (the authority; see the
// construction notes there and in docs/PROTOCOL.md).
//
// Runs on @noble/curves in every context (WebCrypto has no point arithmetic
// or hash-to-curve): pairing is a one-shot exchange, so the pure-JS cost is
// irrelevant, and the implementation is identical in secure and insecure
// browsing contexts. Interop with the Rust host is pinned by
// tests/web-compat.mjs (full handshake against a real nebulad) and the RFC
// 9380 vector test below/in Rust.

import { p256, p256_hasher } from "@noble/curves/nist.js";
import { bytesToNumberBE } from "@noble/curves/utils.js";
import { sha256 as nobleSha256 } from "@noble/hashes/sha2.js";

import { hkdfSha256 } from "./cryptobox";
import { te } from "./protocol";

/** Suite identifier the server advertises in hello_ack.pake. */
export const PAKE_SUITE = "p256-v1";
const PAKE_DST = "NDSP-PAKE-V1-P256_XMD:SHA-256_SSWU_RO_";
const ISK_CONTEXT = te.encode("ndsp-pake-isk-v1");
const PAIR_INFO = te.encode("ndsp-pair-v1");
const SESSION_INFO = te.encode("ndsp-session-v1");

/** 2-byte big-endian length prefix + bytes (injective transcript framing). */
function lp(part: Uint8Array): Uint8Array {
  const out = new Uint8Array(2 + part.length);
  out[0] = (part.length >>> 8) & 0xff;
  out[1] = part.length & 0xff;
  out.set(part, 2);
  return out;
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

export interface PakeShare {
  /** Uncompressed SEC1 share (65 bytes) — the wire format. */
  publicBytes: Uint8Array;
  /**
   * Complete the exchange. `clientShare`/`serverShare` are the exact wire
   * bytes of both shares in role order.
   */
  agree(
    peerBytes: Uint8Array,
    nonce: Uint8Array,
    clientShare: Uint8Array,
    serverShare: Uint8Array,
  ): PakeSecret;
}

export interface PakeSecret {
  pairingKey(salt: Uint8Array, nonce: Uint8Array): Promise<Uint8Array>;
  sessionKey(salt: Uint8Array, nonce: Uint8Array): Promise<Uint8Array>;
}

/** Derive the PIN-bound generator and produce this side's share. */
export function generatePakeShare(
  pin: string,
  nonce: Uint8Array,
  deviceId: string,
  serverFingerprint: string,
): PakeShare {
  const msg = concat(
    lp(te.encode(pin)),
    lp(nonce),
    lp(te.encode(deviceId)),
    lp(te.encode(serverFingerprint)),
  );
  const g = p256_hasher.hashToCurve(msg, { DST: PAKE_DST });
  // Random nonzero scalar in [1, n-1] (randomSecretKey guarantees validity).
  const scalar = bytesToNumberBE(p256.utils.randomSecretKey());
  const publicPoint = g.multiply(scalar);
  const publicBytes = publicPoint.toBytes(false);
  return {
    publicBytes,
    agree(peerBytes, nonceIn, clientShare, serverShare): PakeSecret {
      // fromBytes fully validates: on-curve, canonical, not identity
      // (uncompressed SEC1 cannot encode the identity).
      const peer = p256.Point.fromBytes(peerBytes);
      const k = peer.multiply(scalar); // never identity in a prime-order group
      const kx = k.toBytes(false).subarray(1, 33); // x-coordinate (32 bytes)
      const transcript = concat(
        ISK_CONTEXT,
        lp(nonceIn),
        lp(clientShare),
        lp(serverShare),
        lp(kx),
      );
      const isk = nobleSha256(transcript);
      return {
        pairingKey(salt, n) {
          return hkdfSha256(isk, salt, concat(PAIR_INFO, n));
        },
        sessionKey(salt, n) {
          return hkdfSha256(isk, salt, concat(SESSION_INFO, n));
        },
      };
    },
  };
}

/** Self-check hook used by tests: RFC 9380 P256_XMD:SHA-256_SSWU_RO_ vector. */
export function rfc9380SelfTest(): boolean {
  const p = p256_hasher.hashToCurve(new Uint8Array(0), {
    DST: "QUUX-V01-CS02-with-P256_XMD:SHA-256_SSWU_RO_",
  });
  const enc = p.toBytes(false);
  const xHex = Array.from(enc.subarray(1, 33), (b) => b.toString(16).padStart(2, "0")).join("");
  return xHex === "2c15230b26dbc6fc9a37051158c95b79656e17a1a920b11394ca91c44247d3e4";
}
