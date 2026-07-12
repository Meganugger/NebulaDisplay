// NDSP wire types + envelope framing (mirrors shared/protocol).

import { readU64BE, writeU64BE } from "./caps";
import { AesGcmKey } from "./cryptobox";

export const PROTOCOL_VERSION = 1;
export const WS_PATH = "/ndsp";

export type Codec = "jpeg" | "h264" | "hevc" | "av1";
export type Profile = "office" | "video" | "drawing" | "gaming";
export type InputMode =
  | "view_only"
  | "touchpad"
  | "direct_touch"
  | "keyboard_mouse"
  | "drawing_tablet";

export interface DisplayMode {
  width: number;
  height: number;
  refresh_hz: number;
}

export interface ViewerStats {
  fps_decoded: number;
  decode_ms_avg: number;
  queue_depth: number;
  frames_dropped: number;
  rtt_ms: number;
  e2e_latency_ms: number;
  net_ms_avg: number;
  present_wait_ms_avg: number;
}

export interface HostStats {
  capture_fps: number;
  encode_ms_avg: number;
  target_bitrate_kbps: number;
  actual_bitrate_kbps: number;
  frames_sent: number;
  frames_skipped: number;
  clients: number;
  capture_age_ms_avg: number;
  convert_ms_avg: number;
  seal_send_ms_avg: number;
}

export type TouchPhase = "start" | "move" | "end" | "cancel";

export type InputEvent =
  | { kind: "mouse_move"; x: number; y: number }
  | { kind: "mouse_button"; button: number; pressed: boolean }
  | { kind: "wheel"; dx: number; dy: number }
  | { kind: "key"; code: string; key?: string; pressed: boolean }
  | { kind: "touch"; id: number; phase: TouchPhase; x: number; y: number; pressure: number }
  | { kind: "pen"; phase: TouchPhase; x: number; y: number; pressure: number; tilt_x: number; tilt_y: number }
  | { kind: "text"; text: string };

// Control messages — a permissive structural type keyed on `type`.
export type ControlMsg = { type: string } & Record<string, unknown>;

export const CHANNEL_CONTROL = 1;
export const CHANNEL_VIDEO = 2;

export const DIR_SERVER_TO_CLIENT = 0;
export const DIR_CLIENT_TO_SERVER = 1;

export interface VideoFrame {
  codec: Codec;
  keyframe: boolean;
  seq: number;
  timestampUs: bigint;
  width: number;
  height: number;
  payload: Uint8Array;
}

const CODEC_IDS: Codec[] = ["jpeg", "h264", "hevc", "av1"];

export function parseVideoFrame(buf: Uint8Array): VideoFrame {
  if (buf.length < 18) throw new Error("video frame header truncated");
  const dv = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  const codec = CODEC_IDS[buf[0]!];
  if (!codec) throw new Error(`unknown codec id ${buf[0]}`);
  return {
    codec,
    keyframe: (buf[1]! & 1) !== 0,
    seq: dv.getUint32(2),
    timestampUs: readU64BE(dv, 6),
    width: dv.getUint16(14),
    height: dv.getUint16(16),
    payload: buf.subarray(18),
  };
}

/** Build the 12-byte AES-GCM nonce: [dir, chan, 0, 0, counter u64 BE]. */
function nonceFor(dir: number, chan: number, counter: bigint): Uint8Array {
  const n = new Uint8Array(12);
  n[0] = dir;
  n[1] = chan;
  writeU64BE(new DataView(n.buffer), 4, counter);
  return n;
}

/** Encrypting sender state for one direction. */
export class Sealer {
  private counters = new Map<number, bigint>();
  constructor(
    private key: AesGcmKey,
    private dir: number,
  ) {}

  async seal(chan: number, plaintext: Uint8Array): Promise<Uint8Array> {
    const counter = this.counters.get(chan) ?? 0n;
    this.counters.set(chan, counter + 1n);
    const nonce = nonceFor(this.dir, chan, counter);
    const ct = await this.key.seal(nonce, plaintext, new Uint8Array([chan]));
    const out = new Uint8Array(9 + ct.length);
    out[0] = chan;
    writeU64BE(new DataView(out.buffer), 1, counter);
    out.set(ct, 9);
    return out;
  }
}

/** Decrypting receiver state for the peer's direction. */
export class Opener {
  private nextExpected = new Map<number, bigint>();
  constructor(
    private key: AesGcmKey,
    private dir: number,
  ) {}

  async open(envelope: Uint8Array): Promise<{ chan: number; plaintext: Uint8Array }> {
    if (envelope.length < 1 + 8 + 16) throw new Error("envelope too short");
    const chan = envelope[0]!;
    const counter = readU64BE(new DataView(envelope.buffer, envelope.byteOffset), 1);
    const expected = this.nextExpected.get(chan) ?? 0n;
    if (counter < expected) throw new Error("replayed envelope");
    const nonce = nonceFor(this.dir, chan, counter);
    const pt = await this.key.open(nonce, envelope.subarray(9), new Uint8Array([chan]));
    this.nextExpected.set(chan, counter + 1n);
    return { chan, plaintext: pt };
  }
}

export const te = new TextEncoder();
export const td = new TextDecoder();

export function b64encode(bytes: Uint8Array): string {
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s);
}

export function b64decode(s: string): Uint8Array {
  const bin = atob(s);
  const out = new Uint8Array(bin.length);
  for (let i = 0; i < bin.length; i++) out[i] = bin.charCodeAt(i);
  return out;
}
