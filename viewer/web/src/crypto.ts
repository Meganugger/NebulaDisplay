// Handshake crypto: ECDH P-256 + HKDF-SHA256 + AES-256-GCM.
// Byte-compatible with shared/protocol/src/crypto.rs.
//
// All primitives go through ./cryptobox, which picks native WebCrypto in
// secure contexts and an audited pure-JS fallback in insecure ones (the
// normal plain-HTTP-over-LAN case, where crypto.subtle does not exist).

import { generateUuid, storage } from "./caps";
import {
  AesGcmKey,
  EcdhKeyPair,
  generateEcdhKeys,
  hkdfSha256,
  importAesGcmKey,
  randomNonce,
  sha256,
} from "./cryptobox";
import { b64decode, b64encode, te } from "./protocol";

export const CONFIRM_CONTEXT = te.encode("ndsp-confirm-v1");
const PAIR_INFO = te.encode("ndsp-pair-v1");
const SESSION_INFO = te.encode("ndsp-session-v1");

export interface HandshakeKeys {
  publicRaw: Uint8Array; // uncompressed SEC1 (65 bytes)
  pair: EcdhKeyPair;
}

export async function generateHandshakeKeys(): Promise<HandshakeKeys> {
  const pair = await generateEcdhKeys();
  return { publicRaw: pair.publicRaw, pair };
}

/** ECDH → raw shared secret bits (x-coordinate, 32 bytes). */
export async function agree(keys: HandshakeKeys, peerRaw: Uint8Array): Promise<Uint8Array> {
  return keys.pair.ecdh(peerRaw);
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

export async function pairingKey(
  shared: Uint8Array,
  salt: Uint8Array,
  pin: string,
  nonce: Uint8Array,
): Promise<Uint8Array> {
  return hkdfSha256(shared, salt, concat(PAIR_INFO, te.encode(pin), nonce));
}

export async function sessionKeyBytes(
  shared: Uint8Array,
  salt: Uint8Array,
  nonce: Uint8Array,
): Promise<Uint8Array> {
  return hkdfSha256(shared, salt, concat(SESSION_INFO, nonce));
}

export async function importAesKey(raw: Uint8Array): Promise<AesGcmKey> {
  return importAesGcmKey(raw);
}

/** Seal with random nonce: nonce(12) || ct || tag — matches crypto::seal. */
export async function seal(
  keyRaw: Uint8Array,
  plaintext: Uint8Array,
  aad: Uint8Array,
): Promise<Uint8Array> {
  const key = await importAesGcmKey(keyRaw);
  const nonce = randomNonce();
  const ct = await key.seal(nonce, plaintext, aad);
  return concat(nonce, ct);
}

export async function open(
  keyRaw: Uint8Array,
  sealed: Uint8Array,
  aad: Uint8Array,
): Promise<Uint8Array> {
  const key = await importAesGcmKey(keyRaw);
  return key.open(sealed.subarray(0, 12), sealed.subarray(12), aad);
}

/** Reconnect proof: SHA-256(token || nonce || clientPub || serverPub). */
export async function tokenProof(
  token: Uint8Array,
  nonce: Uint8Array,
  clientPub: Uint8Array,
  serverPub: Uint8Array,
): Promise<Uint8Array> {
  return sha256(concat(token, nonce, clientPub, serverPub));
}

export interface StoredCredentials {
  deviceId: string;
  tokenB64: string;
  hostFingerprint: string;
}

const CREDS_PREFIX = "ndsp.creds.";
const DEVICE_ID_KEY = "ndsp.deviceId";

export function deviceId(): string {
  let id = storage.get(DEVICE_ID_KEY);
  if (!id) {
    id = generateUuid();
    storage.set(DEVICE_ID_KEY, id);
  }
  return id;
}

export function loadCredentials(host: string): StoredCredentials | null {
  const raw = storage.get(CREDS_PREFIX + host);
  if (!raw) return null;
  try {
    return JSON.parse(raw) as StoredCredentials;
  } catch {
    return null;
  }
}

export function saveCredentials(host: string, creds: StoredCredentials): void {
  storage.set(CREDS_PREFIX + host, JSON.stringify(creds));
}

export function clearCredentials(host: string): void {
  storage.remove(CREDS_PREFIX + host);
}

export { b64decode, b64encode };
