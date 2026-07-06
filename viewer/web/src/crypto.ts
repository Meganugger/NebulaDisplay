// Handshake crypto (WebCrypto): ECDH P-256 + HKDF-SHA256 + AES-256-GCM.
// Byte-compatible with shared/protocol/src/crypto.rs.

import { b64decode, b64encode, te } from "./protocol";

export const CONFIRM_CONTEXT = te.encode("ndsp-confirm-v1");
const PAIR_INFO = te.encode("ndsp-pair-v1");
const SESSION_INFO = te.encode("ndsp-session-v1");

export interface HandshakeKeys {
  publicRaw: Uint8Array; // uncompressed SEC1 (65 bytes)
  privateKey: CryptoKey;
}

export async function generateHandshakeKeys(): Promise<HandshakeKeys> {
  const pair = await crypto.subtle.generateKey({ name: "ECDH", namedCurve: "P-256" }, false, [
    "deriveBits",
  ]);
  const publicRaw = new Uint8Array(await crypto.subtle.exportKey("raw", pair.publicKey));
  return { publicRaw, privateKey: pair.privateKey };
}

/** ECDH → raw shared secret bits (x-coordinate, 32 bytes). */
export async function agree(keys: HandshakeKeys, peerRaw: Uint8Array): Promise<Uint8Array> {
  const peer = await crypto.subtle.importKey(
    "raw",
    peerRaw as BufferSource,
    { name: "ECDH", namedCurve: "P-256" },
    false,
    [],
  );
  const bits = await crypto.subtle.deriveBits({ name: "ECDH", public: peer }, keys.privateKey, 256);
  return new Uint8Array(bits);
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

async function hkdf(ikm: Uint8Array, salt: Uint8Array, info: Uint8Array): Promise<Uint8Array> {
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

export async function pairingKey(
  shared: Uint8Array,
  salt: Uint8Array,
  pin: string,
  nonce: Uint8Array,
): Promise<Uint8Array> {
  return hkdf(shared, salt, concat(PAIR_INFO, te.encode(pin), nonce));
}

export async function sessionKeyBytes(
  shared: Uint8Array,
  salt: Uint8Array,
  nonce: Uint8Array,
): Promise<Uint8Array> {
  return hkdf(shared, salt, concat(SESSION_INFO, nonce));
}

export async function importAesKey(raw: Uint8Array): Promise<CryptoKey> {
  return crypto.subtle.importKey("raw", raw as BufferSource, "AES-GCM", false, [
    "encrypt",
    "decrypt",
  ]);
}

/** Seal with random nonce: nonce(12) || ct || tag — matches crypto::seal. */
export async function seal(
  keyRaw: Uint8Array,
  plaintext: Uint8Array,
  aad: Uint8Array,
): Promise<Uint8Array> {
  const key = await importAesKey(keyRaw);
  const nonce = crypto.getRandomValues(new Uint8Array(12));
  const ct = new Uint8Array(
    await crypto.subtle.encrypt(
      { name: "AES-GCM", iv: nonce as BufferSource, additionalData: aad as BufferSource },
      key,
      plaintext as BufferSource,
    ),
  );
  return concat(nonce, ct);
}

export async function open(
  keyRaw: Uint8Array,
  sealed: Uint8Array,
  aad: Uint8Array,
): Promise<Uint8Array> {
  const key = await importAesKey(keyRaw);
  const pt = await crypto.subtle.decrypt(
    {
      name: "AES-GCM",
      iv: sealed.subarray(0, 12) as BufferSource,
      additionalData: aad as BufferSource,
    },
    key,
    sealed.subarray(12) as BufferSource,
  );
  return new Uint8Array(pt);
}

/** Reconnect proof: SHA-256(token || nonce || clientPub || serverPub). */
export async function tokenProof(
  token: Uint8Array,
  nonce: Uint8Array,
  clientPub: Uint8Array,
  serverPub: Uint8Array,
): Promise<Uint8Array> {
  const digest = await crypto.subtle.digest(
    "SHA-256",
    concat(token, nonce, clientPub, serverPub) as BufferSource,
  );
  return new Uint8Array(digest);
}

export interface StoredCredentials {
  deviceId: string;
  tokenB64: string;
  hostFingerprint: string;
}

const CREDS_PREFIX = "ndsp.creds.";
const DEVICE_ID_KEY = "ndsp.deviceId";

export function deviceId(): string {
  let id = localStorage.getItem(DEVICE_ID_KEY);
  if (!id) {
    id = crypto.randomUUID();
    localStorage.setItem(DEVICE_ID_KEY, id);
  }
  return id;
}

export function loadCredentials(host: string): StoredCredentials | null {
  const raw = localStorage.getItem(CREDS_PREFIX + host);
  if (!raw) return null;
  try {
    return JSON.parse(raw) as StoredCredentials;
  } catch {
    return null;
  }
}

export function saveCredentials(host: string, creds: StoredCredentials): void {
  localStorage.setItem(CREDS_PREFIX + host, JSON.stringify(creds));
}

export function clearCredentials(host: string): void {
  localStorage.removeItem(CREDS_PREFIX + host);
}

export { b64decode, b64encode };
