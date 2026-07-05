/**
 * TypeScript mirror of the NebulaDisplay Stream Protocol (NDSP) v1.
 * Wire-compatible with `crates/nebula-proto`. See docs/PROTOCOL.md.
 */

export const PROTOCOL_VERSION = 1;

export const CHANNEL_VIDEO = 0x01;
export const CHANNEL_AUDIO = 0x02;
export const VIDEO_HEADER_LEN = 28;
export const AUDIO_HEADER_LEN = 20;

export const CAP_VIDEO_MJPEG = "video/mjpeg";
export const CAP_VIDEO_H264 = "video/h264";
export const CAP_INPUT = "input";

export type Profile = "office" | "video" | "drawing" | "gaming" | "balanced";
export type DisplayMode = "mirror" | "extend";

export interface VideoModeInfo {
  width: number;
  height: number;
  refresh_hz: number;
}

export interface StreamStats {
  fps: number;
  bitrate_kbps: number;
  encode_ms: number;
  capture_ms: number;
  rtt_ms: number;
  quality: number;
  frames_sent: number;
  frames_dropped: number;
  width: number;
  height: number;
}

export type InputEvent =
  | { kind: "mouse_move"; x: number; y: number }
  | { kind: "mouse_button"; button: "left" | "right" | "middle" | "back" | "forward"; down: boolean; x: number; y: number }
  | { kind: "mouse_wheel"; dx: number; dy: number }
  | { kind: "key"; code: string; down: boolean }
  | { kind: "touch"; id: number; phase: "down" | "move" | "up" | "cancel"; x: number; y: number; pressure: number | null }
  | { kind: "stylus"; x: number; y: number; pressure: number; tilt_x: number | null; tilt_y: number | null; down: boolean; eraser: boolean };

/** JSON control messages (tagged with `type`). */
export type ControlMessage =
  | { type: "hello"; min_version: number; max_version: number; client_name: string; device_id: string; capabilities: string[] }
  | { type: "hello_ack"; version: number; host_name: string; capabilities: string[]; known_device: boolean }
  | { type: "pair_request"; pin: string; device_name: string }
  | { type: "pair_ok"; token: string }
  | { type: "auth"; token: string }
  | { type: "auth_ok"; input_allowed: boolean }
  | { type: "session_start"; mode: DisplayMode; profile: Profile; preferred: VideoModeInfo | null; viewport_width: number; viewport_height: number; codecs: string[]; want_audio: boolean }
  | { type: "session_started"; codec: string; mode: VideoModeInfo; display_mode: DisplayMode; audio: boolean; monitor_index: number }
  | { type: "session_stop"; reason: string }
  | { type: "mode_change"; preferred: VideoModeInfo | null; profile: Profile | null }
  | { type: "input"; events: InputEvent[] }
  | { type: "input_permission"; allowed: boolean }
  | { type: "ping"; t_micros: number }
  | { type: "pong"; t_micros: number }
  | { type: "feedback"; last_presented_frame: number; dropped_frames: number; decode_ms: number; queue_depth: number }
  | { type: "stats" } & StreamStats
  | { type: "error"; code: string; message: string }
  | { type: "bye"; resume_token: string | null }
  | { type: "resume"; resume_token: string; last_frame: number }
  | { type: "resume_ok"; from_frame: number };

export interface VideoPacket {
  codec: number;
  fullFrame: boolean;
  keyframe: boolean;
  frameId: number;
  captureTsMicros: number;
  x: number;
  y: number;
  w: number;
  h: number;
  streamW: number;
  streamH: number;
  payload: Uint8Array;
}

/** Parse a binary video packet. Returns null for non-video channels. */
export function decodeVideoPacket(buf: ArrayBuffer): VideoPacket | null {
  const view = new DataView(buf);
  if (buf.byteLength < VIDEO_HEADER_LEN || view.getUint8(0) !== CHANNEL_VIDEO) return null;
  if (view.getUint8(1) !== 1) {
    console.warn("unsupported video packet version", view.getUint8(1));
    return null;
  }
  const flags = view.getUint8(3);
  return {
    codec: view.getUint8(2),
    fullFrame: (flags & 1) !== 0,
    keyframe: (flags & 2) !== 0,
    frameId: view.getUint32(4, true),
    captureTsMicros: Number(view.getBigUint64(8, true)),
    x: view.getUint16(16, true),
    y: view.getUint16(18, true),
    w: view.getUint16(20, true),
    h: view.getUint16(22, true),
    streamW: view.getUint16(24, true),
    streamH: view.getUint16(26, true),
    payload: new Uint8Array(buf, VIDEO_HEADER_LEN),
  };
}

export interface AudioPacket {
  codec: number;
  channels: number;
  seq: number;
  captureTsMicros: number;
  sampleRate: number;
  payload: Uint8Array;
}

export function decodeAudioPacket(buf: ArrayBuffer): AudioPacket | null {
  const view = new DataView(buf);
  if (buf.byteLength < AUDIO_HEADER_LEN || view.getUint8(0) !== CHANNEL_AUDIO) return null;
  if (view.getUint8(1) !== 1) return null;
  return {
    codec: view.getUint8(2),
    channels: view.getUint8(3),
    seq: view.getUint32(4, true),
    captureTsMicros: Number(view.getBigUint64(8, true)),
    sampleRate: view.getUint32(16, true),
    payload: new Uint8Array(buf, AUDIO_HEADER_LEN),
  };
}

/** Stable per-browser device id. */
export function deviceId(): string {
  const KEY = "nebula.device_id";
  let id = localStorage.getItem(KEY);
  if (!id) {
    const bytes = new Uint8Array(16);
    crypto.getRandomValues(bytes);
    id = Array.from(bytes, (b) => b.toString(16).padStart(2, "0")).join("");
    localStorage.setItem(KEY, id);
  }
  return id;
}
