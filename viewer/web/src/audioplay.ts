// Host-audio playback: Opus packets (channel 3) → WebCodecs AudioDecoder →
// WebAudio scheduling with a small jitter buffer.
//
// Requires a secure context (AudioDecoder, like VideoDecoder, does not exist
// on plain-HTTP origins) — the session only advertises the "audio" feature
// when it's present. The AudioContext is created from the toolbar click that
// enables audio, which satisfies autoplay policies.

import { AudioFrameMsg } from "./protocol";

/** Schedule this far ahead of the context clock (jitter absorption). */
const LEAD_S = 0.06;
/** If we fall further behind than this, resync instead of chasing. */
const MAX_LAG_S = 0.25;

export class AudioPlayer {
  private ctx: AudioContext | null = null;
  private gain: GainNode | null = null;
  private decoder: AudioDecoder | null = null;
  private playhead = 0;
  private baseTsUs: bigint | null = null;
  private closed = false;
  packetsDecoded = 0;

  static supported(): boolean {
    return typeof AudioDecoder === "function" && typeof AudioContext === "function";
  }

  /** Call from a user gesture (toolbar click). */
  start(sampleRate: number, channels: number, volume: number): void {
    this.ctx = new AudioContext({ sampleRate });
    this.gain = this.ctx.createGain();
    this.gain.gain.value = volume;
    this.gain.connect(this.ctx.destination);
    this.playhead = 0;
    this.decoder = new AudioDecoder({
      output: (data) => this.schedule(data),
      error: (e) => console.error("audio decode error", e),
    });
    this.decoder.configure({
      codec: "opus",
      sampleRate,
      numberOfChannels: channels,
    });
  }

  push(frame: AudioFrameMsg): void {
    if (!this.decoder || this.decoder.state !== "configured") return;
    if (this.baseTsUs === null) this.baseTsUs = frame.timestampUs;
    // Opus packets are independently decodable → every chunk is a "key".
    this.decoder.decode(
      new EncodedAudioChunk({
        type: "key",
        timestamp: Number(frame.timestampUs - this.baseTsUs),
        data: frame.payload as BufferSource,
      }),
    );
  }

  private schedule(data: AudioData): void {
    const ctx = this.ctx;
    const gain = this.gain;
    if (!ctx || !gain || this.closed) {
      data.close();
      return;
    }
    try {
      const frames = data.numberOfFrames;
      const channels = data.numberOfChannels;
      const buf = ctx.createBuffer(channels, frames, data.sampleRate);
      for (let ch = 0; ch < channels; ch++) {
        const plane = new Float32Array(frames);
        data.copyTo(plane, { planeIndex: ch, format: "f32-planar" });
        buf.copyToChannel(plane, ch);
      }
      const now = ctx.currentTime;
      // Resync after long stalls; otherwise keep a steady LEAD_S buffer.
      if (this.playhead < now + 0.005 || this.playhead > now + MAX_LAG_S + LEAD_S) {
        this.playhead = now + LEAD_S;
      }
      const src = ctx.createBufferSource();
      src.buffer = buf;
      src.connect(gain);
      src.start(this.playhead);
      this.playhead += buf.duration;
      this.packetsDecoded++;
    } catch (e) {
      console.warn("audio schedule failed", e);
    } finally {
      data.close();
    }
  }

  setVolume(v: number): void {
    if (this.gain) this.gain.gain.value = Math.min(1, Math.max(0, v));
  }

  stop(): void {
    this.closed = true;
    try {
      this.decoder?.close();
    } catch {
      /* already closed */
    }
    this.decoder = null;
    void this.ctx?.close();
    this.ctx = null;
    this.gain = null;
    this.baseTsUs = null;
  }
}
