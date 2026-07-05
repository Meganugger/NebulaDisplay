/**
 * NebulaClient: connection + protocol state machine for the browser viewer.
 *
 * Responsibilities: WebSocket lifecycle, hello/pair/auth, session start,
 * decode-and-present of video packets, PCM audio playback, input batching,
 * client feedback, and automatic reconnect with the stored device token.
 */

import {
  CAP_INPUT,
  CAP_VIDEO_MJPEG,
  ControlMessage,
  decodeAudioPacket,
  decodeVideoPacket,
  deviceId,
  InputEvent,
  Profile,
  PROTOCOL_VERSION,
  StreamStats,
} from "./protocol";

export type ClientState =
  | "disconnected"
  | "connecting"
  | "hello"
  | "need_pairing"
  | "authenticating"
  | "ready"
  | "streaming";

export interface ClientEvents {
  state: (s: ClientState, detail?: string) => void;
  needPin: () => void;
  paired: () => void;
  frame: (frameId: number) => void;
  stats: (host: StreamStats, client: ClientSideStats) => void;
  inputPermission: (allowed: boolean) => void;
  error: (code: string, message: string) => void;
  sessionInfo: (width: number, height: number, audio: boolean) => void;
}

export interface ClientSideStats {
  fpsPresented: number;
  bitrateKbps: number;
  decodeMs: number;
  rttMs: number;
  droppedFrames: number;
}

const TOKEN_KEY_PREFIX = "nebula.token.";

export class NebulaClient {
  private ws: WebSocket | null = null;
  private canvas: HTMLCanvasElement;
  private ctx: CanvasRenderingContext2D;
  private handlers: Partial<ClientEvents> = {};
  private state: ClientState = "disconnected";
  private hostKey = "";
  private wsUrl = "";
  private profile: Profile = "balanced";
  private wantAudio = false;

  // Input batching.
  private inputQueue: InputEvent[] = [];
  private inputTimer: number | null = null;
  inputAllowed = false;

  // Presentation / stats.
  private presented = 0;
  private dropped = 0;
  private bytesReceived = 0;
  private decodeMsEma = 0;
  private rttMs = 0;
  private lastFrameId = 0;
  private decoding = false;
  private pendingPacket: ArrayBuffer | null = null;
  private statsTimer: number | null = null;
  private windowStart = performance.now();
  private windowFrames = 0;
  private windowBytes = 0;
  private clientStats: ClientSideStats = { fpsPresented: 0, bitrateKbps: 0, decodeMs: 0, rttMs: 0, droppedFrames: 0 };

  // Audio.
  private audioCtx: AudioContext | null = null;
  private audioTime = 0;

  private reconnectAttempts = 0;
  private closedByUser = false;

  constructor(canvas: HTMLCanvasElement) {
    this.canvas = canvas;
    const ctx = canvas.getContext("2d");
    if (!ctx) throw new Error("2D canvas unsupported");
    this.ctx = ctx;
  }

  on<K extends keyof ClientEvents>(event: K, fn: ClientEvents[K]): void {
    this.handlers[event] = fn;
  }

  private emitState(s: ClientState, detail?: string) {
    this.state = s;
    this.handlers.state?.(s, detail);
  }

  getState(): ClientState {
    return this.state;
  }

  /** Connect to a host. `hostKey` identifies the token slot (host:port). */
  connect(wsUrl: string, opts: { profile: Profile; wantAudio: boolean }): void {
    this.closedByUser = false;
    this.wsUrl = wsUrl;
    this.profile = opts.profile;
    this.wantAudio = opts.wantAudio;
    this.hostKey = wsUrl.replace(/^wss?:\/\//, "").replace(/\/.*$/, "");
    this.open();
  }

  private open(): void {
    this.emitState("connecting");
    const ws = new WebSocket(this.wsUrl);
    ws.binaryType = "arraybuffer";
    this.ws = ws;

    ws.onopen = () => {
      this.reconnectAttempts = 0;
      this.emitState("hello");
      this.send({
        type: "hello",
        min_version: 1,
        max_version: PROTOCOL_VERSION,
        client_name: `Web viewer (${browserName()})`,
        device_id: deviceId(),
        capabilities: [CAP_VIDEO_MJPEG, CAP_INPUT],
      });
    };

    ws.onmessage = (ev) => {
      if (typeof ev.data === "string") {
        this.onControl(JSON.parse(ev.data) as ControlMessage);
      } else {
        this.onBinary(ev.data as ArrayBuffer);
      }
    };

    ws.onclose = () => {
      this.stopTimers();
      if (this.closedByUser) {
        this.emitState("disconnected");
        return;
      }
      // Automatic reconnect with backoff (network blip / host restart).
      const delay = Math.min(500 * 2 ** this.reconnectAttempts, 8000);
      this.reconnectAttempts++;
      this.emitState("connecting", `reconnecting in ${(delay / 1000).toFixed(1)}s`);
      window.setTimeout(() => {
        if (!this.closedByUser) this.open();
      }, delay);
    };

    ws.onerror = () => {
      // onclose fires next; nothing to do here.
    };
  }

  disconnect(): void {
    this.closedByUser = true;
    this.send({ type: "bye", resume_token: null });
    this.ws?.close();
    this.stopTimers();
    this.emitState("disconnected");
  }

  pair(pin: string): void {
    this.send({ type: "pair_request", pin, device_name: `Browser on ${platformName()}` });
  }

  setProfile(profile: Profile): void {
    this.profile = profile;
    if (this.state === "streaming") {
      this.send({ type: "mode_change", preferred: null, profile });
    }
  }

  sendInput(ev: InputEvent): void {
    if (this.state !== "streaming" || !this.inputAllowed) return;
    this.inputQueue.push(ev);
    // Batch input at ~120Hz to keep message overhead low without adding
    // perceptible latency.
    this.inputTimer ??= window.setTimeout(() => {
      this.inputTimer = null;
      if (this.inputQueue.length > 0) {
        this.send({ type: "input", events: this.inputQueue });
        this.inputQueue = [];
      }
    }, 8);
  }

  private send(msg: ControlMessage): void {
    if (this.ws?.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify(msg));
    }
  }

  private token(): string | null {
    return localStorage.getItem(TOKEN_KEY_PREFIX + this.hostKey);
  }

  private storeToken(token: string): void {
    localStorage.setItem(TOKEN_KEY_PREFIX + this.hostKey, token);
  }

  forgetToken(): void {
    localStorage.removeItem(TOKEN_KEY_PREFIX + this.hostKey);
  }

  private onControl(msg: ControlMessage): void {
    switch (msg.type) {
      case "hello_ack": {
        const token = this.token();
        if (msg.known_device && token) {
          this.emitState("authenticating");
          this.send({ type: "auth", token });
        } else {
          this.emitState("need_pairing");
          this.handlers.needPin?.();
        }
        break;
      }
      case "pair_ok":
        this.storeToken(msg.token);
        this.handlers.paired?.();
        this.startSession();
        break;
      case "auth_ok":
        this.inputAllowed = msg.input_allowed;
        this.handlers.inputPermission?.(msg.input_allowed);
        this.startSession();
        break;
      case "session_started":
        this.emitState("streaming");
        this.canvas.width = msg.mode.width;
        this.canvas.height = msg.mode.height;
        this.handlers.sessionInfo?.(msg.mode.width, msg.mode.height, msg.audio);
        this.startTimers();
        break;
      case "session_stop":
        this.emitState("ready", msg.reason);
        break;
      case "input_permission":
        this.inputAllowed = msg.allowed;
        this.handlers.inputPermission?.(msg.allowed);
        break;
      case "ping":
        this.send({ type: "pong", t_micros: msg.t_micros });
        break;
      case "pong": {
        // We timestamp pings with performance.now() microseconds.
        const rtt = performance.now() - msg.t_micros / 1000;
        if (rtt >= 0 && rtt < 30000) this.rttMs = rtt;
        break;
      }
      case "stats":
        this.clientStats = {
          fpsPresented: this.windowRate(),
          bitrateKbps: this.windowBitrate(),
          decodeMs: this.decodeMsEma,
          rttMs: this.rttMs,
          droppedFrames: this.dropped,
        };
        this.handlers.stats?.(msg as unknown as StreamStats, this.clientStats);
        break;
      case "error":
        if (msg.code === "bad_token") {
          // Stored token was revoked — forget it and fall back to pairing.
          this.forgetToken();
          this.emitState("need_pairing");
          this.handlers.needPin?.();
        }
        this.handlers.error?.(msg.code, msg.message);
        break;
      default:
        break;
    }
  }

  private startSession(): void {
    this.emitState("ready");
    const dpr = window.devicePixelRatio || 1;
    this.send({
      type: "session_start",
      mode: "mirror",
      profile: this.profile,
      preferred: null,
      viewport_width: Math.round(this.canvas.clientWidth * dpr) || screen.width,
      viewport_height: Math.round(this.canvas.clientHeight * dpr) || screen.height,
      codecs: [CAP_VIDEO_MJPEG],
      want_audio: this.wantAudio,
    });
  }

  // -------------------------------------------------------------------
  // Media path
  // -------------------------------------------------------------------

  private onBinary(buf: ArrayBuffer): void {
    this.bytesReceived += buf.byteLength;
    this.windowBytes += buf.byteLength;
    const video = decodeVideoPacket(buf);
    if (video) {
      // Keep at most one pending packet: if decode falls behind, newer
      // frames replace older undecoded ones (latest-wins, low latency).
      if (this.decoding) {
        if (this.pendingPacket) this.dropped++;
        this.pendingPacket = buf;
      } else {
        void this.decodeAndPresent(buf);
      }
      return;
    }
    const audio = decodeAudioPacket(buf);
    if (audio) this.playAudio(audio.payload, audio.channels, audio.sampleRate);
  }

  private async decodeAndPresent(buf: ArrayBuffer): Promise<void> {
    this.decoding = true;
    try {
      const pkt = decodeVideoPacket(buf);
      if (!pkt) return;
      const t0 = performance.now();
      const payload = pkt.payload.slice(); // copy into a plain ArrayBuffer for Blob
      const blob = new Blob([payload.buffer as ArrayBuffer], { type: "image/jpeg" });
      const bitmap = await createImageBitmap(blob);
      if (this.canvas.width !== pkt.streamW || this.canvas.height !== pkt.streamH) {
        this.canvas.width = pkt.streamW;
        this.canvas.height = pkt.streamH;
      }
      this.ctx.drawImage(bitmap, pkt.x, pkt.y);
      bitmap.close();
      const dt = performance.now() - t0;
      this.decodeMsEma = this.decodeMsEma === 0 ? dt : this.decodeMsEma * 0.9 + dt * 0.1;
      this.presented++;
      this.windowFrames++;
      this.lastFrameId = pkt.frameId;
      this.handlers.frame?.(pkt.frameId);
    } finally {
      this.decoding = false;
      const next = this.pendingPacket;
      this.pendingPacket = null;
      if (next) void this.decodeAndPresent(next);
    }
  }

  private playAudio(pcm: Uint8Array, channels: number, sampleRate: number): void {
    this.audioCtx ??= new AudioContext({ sampleRate });
    const ctx = this.audioCtx;
    const frames = pcm.byteLength / 2 / channels;
    if (frames <= 0) return;
    const buffer = ctx.createBuffer(channels, frames, sampleRate);
    const view = new DataView(pcm.buffer, pcm.byteOffset, pcm.byteLength);
    for (let ch = 0; ch < channels; ch++) {
      const data = buffer.getChannelData(ch);
      for (let i = 0; i < frames; i++) {
        data[i] = view.getInt16((i * channels + ch) * 2, true) / 32768;
      }
    }
    const src = ctx.createBufferSource();
    src.buffer = buffer;
    src.connect(ctx.destination);
    // Schedule seamlessly after the previous chunk (small jitter cushion).
    const startAt = Math.max(ctx.currentTime + 0.03, this.audioTime);
    src.start(startAt);
    this.audioTime = startAt + buffer.duration;
  }

  // -------------------------------------------------------------------
  // Periodic feedback + RTT probes
  // -------------------------------------------------------------------

  private startTimers(): void {
    this.stopTimers();
    this.windowStart = performance.now();
    this.statsTimer = window.setInterval(() => {
      this.send({ type: "ping", t_micros: Math.round(performance.now() * 1000) });
      this.send({
        type: "feedback",
        last_presented_frame: this.lastFrameId,
        dropped_frames: this.dropped,
        decode_ms: this.decodeMsEma,
        queue_depth: this.pendingPacket ? 1 : 0,
      });
      this.dropped = 0;
      this.windowStart = performance.now();
      this.windowFrames = 0;
      this.windowBytes = 0;
    }, 1000);
  }

  private windowRate(): number {
    const secs = (performance.now() - this.windowStart) / 1000;
    return secs > 0 ? this.windowFrames / secs : 0;
  }

  private windowBitrate(): number {
    const secs = (performance.now() - this.windowStart) / 1000;
    return secs > 0 ? (this.windowBytes * 8) / secs / 1000 : 0;
  }

  private stopTimers(): void {
    if (this.statsTimer !== null) {
      clearInterval(this.statsTimer);
      this.statsTimer = null;
    }
  }
}

function browserName(): string {
  const ua = navigator.userAgent;
  if (ua.includes("Firefox/")) return "Firefox";
  if (ua.includes("Edg/")) return "Edge";
  if (ua.includes("Chrome/")) return "Chrome";
  if (ua.includes("Safari/")) return "Safari";
  return "Browser";
}

function platformName(): string {
  const ua = navigator.userAgent;
  if (/Android/.test(ua)) return "Android";
  if (/iPhone|iPad/.test(ua)) return "iOS";
  if (/Windows/.test(ua)) return "Windows";
  if (/Mac/.test(ua)) return "macOS";
  if (/Linux/.test(ua)) return "Linux";
  return "device";
}
