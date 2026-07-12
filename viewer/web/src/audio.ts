// Host-audio playback: encrypted channel 3 → WebCodecs AudioDecoder (Opus)
// → Web Audio scheduling with a small jitter buffer.
//
// Requires WebCodecs AudioDecoder (Chromium ≥94, Safari ≥16.4 — secure
// contexts). Where it's missing (plain-HTTP LAN origins, Firefox), the audio
// button is disabled with an explanatory tooltip; there is no JS Opus
// decoder fallback bundled (≈100 KB of WASM for a secondary feature — see
// docs/BROWSER-COMPAT.md).

import { AudioFrame } from "./protocol";

export function audioPlaybackSupported(): boolean {
  return typeof AudioDecoder !== "undefined" && typeof AudioContext !== "undefined";
}

/** Target scheduling cushion. Small enough to stay "live", large enough to
 *  absorb network + decoder jitter without crackling. */
const JITTER_S = 0.06;

export class AudioPlayer {
  private ctx: AudioContext;
  private gain: GainNode;
  private decoder: AudioDecoder | null = null;
  private nextTime = 0;
  private channels = 2;
  private closed = false;
  /** Diagnostics: decode failures (decoder is re-created on error). */
  errors = 0;

  constructor() {
    this.ctx = new AudioContext({ latencyHint: "interactive", sampleRate: 48000 });
    this.gain = this.ctx.createGain();
    this.gain.connect(this.ctx.destination);
  }

  /** Call from a user gesture (autoplay policy). */
  async resume(): Promise<void> {
    if (this.ctx.state === "suspended") await this.ctx.resume();
  }

  set volume(v: number) {
    this.gain.gain.value = Math.max(0, Math.min(1, v));
  }

  push(frame: AudioFrame): void {
    if (this.closed) return;
    if (!this.decoder || this.channels !== frame.channels) {
      this.channels = frame.channels;
      this.decoder?.close();
      this.decoder = new AudioDecoder({
        output: (data) => this.schedule(data),
        error: (e) => {
          console.warn("audio decode error", e);
          this.errors++;
          // Surfaced for the E2E harness.
          (globalThis as Record<string, unknown>)["__ndspAudioErrors"] = this.errors;
          this.decoder = null; // re-created on the next packet
        },
      });
      this.decoder.configure({
        codec: "opus",
        sampleRate: frame.sampleRate,
        numberOfChannels: frame.channels,
      });
    }
    // Every Opus packet is independently decodable.
    this.decoder.decode(
      new EncodedAudioChunk({
        type: "key",
        timestamp: Number(frame.timestampUs),
        data: frame.payload as BufferSource,
      }),
    );
  }

  private schedule(data: AudioData): void {
    try {
      if (this.closed) return;
      const frames = data.numberOfFrames;
      const chans = Math.min(data.numberOfChannels, 2);
      const buf = this.ctx.createBuffer(chans, frames, data.sampleRate);
      for (let c = 0; c < chans; c++) {
        const plane = new Float32Array(frames);
        data.copyTo(plane, { planeIndex: c, format: "f32-planar" });
        buf.getChannelData(c).set(plane);
      }
      const now = this.ctx.currentTime;
      // Underrun (gap in the stream) → rebuild the cushion; otherwise chain
      // buffers seamlessly.
      if (this.nextTime < now + 0.01) this.nextTime = now + JITTER_S;
      const src = this.ctx.createBufferSource();
      src.buffer = buf;
      src.connect(this.gain);
      src.start(this.nextTime);
      this.nextTime += frames / data.sampleRate;
    } finally {
      data.close();
    }
  }

  close(): void {
    this.closed = true;
    try {
      this.decoder?.close();
    } catch {
      /* already closed */
    }
    this.decoder = null;
    void this.ctx.close();
  }
}
