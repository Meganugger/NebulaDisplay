// Opus audio playback: WebCodecs AudioDecoder → Web Audio scheduling.
//
// Requires a secure context (AudioDecoder is WebCodecs); on plain-HTTP LAN
// pages the audio toggle is hidden — video/input work as before. Playback
// keeps a small jitter buffer (~60 ms) and resets cleanly after network gaps
// (dropped packets show up as seq gaps and a short glitch, never as
// accumulating delay).

import { caps } from "./caps";
import { AudioFrame, AUDIO_CODEC_OPUS } from "./protocol";

/** Target scheduling headroom. Below this we re-anchor (gap → glitch). */
const JITTER_S = 0.06;
/** Max headroom before we drop a packet to stop latency creep. */
const MAX_BUFFER_S = 0.3;

export function audioPlaybackSupported(): boolean {
  return caps.audioDecoder && caps.audioContext;
}

export class AudioPlayer {
  private ctx: AudioContext;
  private gain: GainNode;
  private decoder: AudioDecoder | null = null;
  private nextPlayTime = 0;
  private lastSeq = 0;
  /** Sequence gaps observed (network drops), for the stats overlay. */
  packetsLost = 0;
  packetsPlayed = 0;
  onError: (e: Error) => void = () => {};

  /** Must be constructed from a user gesture (AudioContext autoplay rules). */
  constructor() {
    this.ctx = new AudioContext({ latencyHint: "interactive" });
    this.gain = this.ctx.createGain();
    this.gain.connect(this.ctx.destination);
  }

  get volume(): number {
    return this.gain.gain.value;
  }

  set volume(v: number) {
    this.gain.gain.value = Math.min(1, Math.max(0, v));
  }

  push(frame: AudioFrame): void {
    if (frame.codec !== AUDIO_CODEC_OPUS) return; // unknown codec id — skip
    if (this.lastSeq !== 0 && frame.seq > this.lastSeq + 1) {
      this.packetsLost += frame.seq - this.lastSeq - 1;
    }
    this.lastSeq = frame.seq;
    if (!this.decoder || this.decoder.state === "closed") {
      this.configure(frame.sampleRate, frame.channels);
    }
    try {
      this.decoder!.decode(
        new EncodedAudioChunk({
          type: "key",
          timestamp: Number(frame.timestampUs),
          data: frame.payload as BufferSource,
        }),
      );
    } catch (e) {
      this.onError(e as Error);
    }
  }

  private configure(sampleRate: number, channels: number): void {
    this.decoder = new AudioDecoder({
      output: (data) => this.schedule(data),
      error: (e) => this.onError(new Error(`audio decode: ${e.message}`)),
    });
    this.decoder.configure({
      codec: "opus",
      sampleRate,
      numberOfChannels: channels,
    });
  }

  /** Convert one decoded AudioData to an AudioBuffer and time it in. */
  private schedule(data: AudioData): void {
    try {
      const frames = data.numberOfFrames;
      const channels = data.numberOfChannels;
      const buf = this.ctx.createBuffer(channels, frames, data.sampleRate);
      for (let ch = 0; ch < channels; ch++) {
        // copyTo with f32-planar converts from whatever the decoder produced.
        data.copyTo(buf.getChannelData(ch) as unknown as BufferSource, {
          planeIndex: ch,
          format: "f32-planar",
        });
      }
      const now = this.ctx.currentTime;
      if (this.nextPlayTime < now + JITTER_S / 2) {
        // Fell behind (gap / tab throttling): re-anchor with jitter headroom.
        this.nextPlayTime = now + JITTER_S;
      } else if (this.nextPlayTime > now + MAX_BUFFER_S) {
        // Too far ahead (clock drift / burst): drop to stop latency creep.
        return;
      }
      const src = this.ctx.createBufferSource();
      src.buffer = buf;
      src.connect(this.gain);
      src.start(this.nextPlayTime);
      this.nextPlayTime += buf.duration;
      this.packetsPlayed++;
    } finally {
      data.close();
    }
  }

  /** Buffered play-ahead in ms (stats overlay). */
  get bufferedMs(): number {
    return Math.max(0, (this.nextPlayTime - this.ctx.currentTime) * 1000);
  }

  close(): void {
    try {
      this.decoder?.close();
    } catch {
      /* already closed */
    }
    void this.ctx.close();
  }
}
