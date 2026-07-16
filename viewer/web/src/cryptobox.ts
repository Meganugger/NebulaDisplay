// Unified crypto primitives with capability-based backend selection.
//
// Secure contexts (https / localhost) use native WebCrypto. Insecure contexts
// — i.e. the normal case of the viewer served over plain-HTTP LAN, where
// crypto.subtle simply does not exist — use audited pure-JS implementations
// (@noble/curves, @noble/hashes, @noble/ciphers). Both backends are verified
// byte-compatible with each other and with shared/protocol/src/crypto.rs by
// tests/web-compat.mjs (which runs the handshake against the real Rust host
// on both backends).

import { gcm } from "@noble/ciphers/aes.js";
import { p256 } from "@noble/curves/nist.js";
import { hkdf as nobleHkdf } from "@noble/hashes/hkdf.js";
import { sha256 as nobleSha256 } from "@noble/hashes/sha2.js";

import { caps } from "./caps";

/** True when the native WebCrypto backend is in use. */
export const usingNativeCrypto: boolean = caps.subtle;

export interface EcdhKeyPair {
  /** Uncompressed SEC1 public key (65 bytes) — the NDSP wire format. */
  publicRaw: Uint8Array;
  /** ECDH → 32-byte shared secret (curve-point x-coordinate). */
  ecdh(peerRaw: Uint8Array): Promise<Uint8Array>;
}

export interface AesGcmKey {
  /** AES-256-GCM encrypt → ciphertext || 16-byte tag. */
  seal(nonce: Uint8Array, plaintext: Uint8Array, aad: Uint8Array): Promise<Uint8Array>;
  /** AES-256-GCM decrypt of ciphertext || tag. Throws on auth failure. */
  open(nonce: Uint8Array, sealed: Uint8Array, aad: Uint8Array): Promise<Uint8Array>;
}

export async function generateEcdhKeys(): Promise<EcdhKeyPair> {
  if (usingNativeCrypto) {
    const pair = await crypto.subtle.generateKey({ name: "ECDH", namedCurve: "P-256" }, false, [
      "deriveBits",
    ]);
    const publicRaw = new Uint8Array(await crypto.subtle.exportKey("raw", pair.publicKey));
    return {
      publicRaw,
      async ecdh(peerRaw: Uint8Array): Promise<Uint8Array> {
        const peer = await crypto.subtle.importKey(
          "raw",
          peerRaw as BufferSource,
          { name: "ECDH", namedCurve: "P-256" },
          false,
          [],
        );
        const bits = await crypto.subtle.deriveBits(
          { name: "ECDH", public: peer },
          pair.privateKey,
          256,
        );
        return new Uint8Array(bits);
      },
    };
  }
  // Pure-JS fallback. Private scalar stays inside this closure.
  const priv = p256.utils.randomSecretKey();
  const publicRaw = p256.getPublicKey(priv, false);
  return {
    publicRaw,
    ecdh(peerRaw: Uint8Array): Promise<Uint8Array> {
      // Uncompressed shared point = 0x04 || x(32) || y(32); WebCrypto's
      // deriveBits (and the Rust host) use the x-coordinate.
      const point = p256.getSharedSecret(priv, peerRaw, false);
      return Promise.resolve(point.subarray(1, 33));
    },
  };
}

export async function hkdfSha256(
  ikm: Uint8Array,
  salt: Uint8Array,
  info: Uint8Array,
): Promise<Uint8Array> {
  if (usingNativeCrypto) {
    const key = await crypto.subtle.importKey("raw", ikm as BufferSource, "HKDF", false, [
      "deriveBits",
    ]);
    const bits = await crypto.subtle.deriveBits(
      { name: "HKDF", hash: "SHA-256", salt: salt as BufferSource, info: info as BufferSource },
      key,
      256,
    );
    return new Uint8Array(bits);
  }
  return nobleHkdf(nobleSha256, ikm, salt, info, 32);
}

/** Incremental SHA-256 for large inputs (file transfers hash gigabytes —
 *  WebCrypto digest() would need the whole file in memory). */
export function sha256Incremental(): { update(d: Uint8Array): void; digest(): Uint8Array } {
  const h = nobleSha256.create();
  return { update: (d) => void h.update(d), digest: () => h.digest() };
}

export async function sha256(data: Uint8Array): Promise<Uint8Array> {
  if (usingNativeCrypto) {
    return new Uint8Array(await crypto.subtle.digest("SHA-256", data as BufferSource));
  }
  return nobleSha256(data);
}

/**
 * Import a raw 256-bit key. The native backend imports the CryptoKey once and
 * reuses it for every envelope (hot path: one call per video frame).
 */
export async function importAesGcmKey(raw: Uint8Array): Promise<AesGcmKey> {
  if (usingNativeCrypto) {
    const key = await crypto.subtle.importKey("raw", raw as BufferSource, "AES-GCM", false, [
      "encrypt",
      "decrypt",
    ]);
    return {
      async seal(nonce, plaintext, aad): Promise<Uint8Array> {
        const ct = await crypto.subtle.encrypt(
          { name: "AES-GCM", iv: nonce as BufferSource, additionalData: aad as BufferSource },
          key,
          plaintext as BufferSource,
        );
        return new Uint8Array(ct);
      },
      async open(nonce, sealed, aad): Promise<Uint8Array> {
        const pt = await crypto.subtle.decrypt(
          { name: "AES-GCM", iv: nonce as BufferSource, additionalData: aad as BufferSource },
          key,
          sealed as BufferSource,
        );
        return new Uint8Array(pt);
      },
    };
  }
  const keyCopy = raw.slice(); // detach from any shared buffer
  return {
    seal(nonce, plaintext, aad): Promise<Uint8Array> {
      return Promise.resolve(gcm(keyCopy, nonce, aad).encrypt(plaintext));
    },
    open(nonce, sealed, aad): Promise<Uint8Array> {
      return Promise.resolve(gcm(keyCopy, nonce, aad).decrypt(sealed));
    },
  };
}

