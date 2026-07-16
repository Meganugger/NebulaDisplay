// Audio playback: Opus via WebCodecs AudioDecoder (secure contexts), raw
// PCM via AudioContext everywhere else (insecure plain-HTTP LAN origins
// have no WebCodecs — the host sends s16le instead, trivial on a LAN).
//
// Scheduling: decoded blocks are appended to a rolling AudioContext
// timeline with a small jitter buffer. Gaps (dropped packets, tab throttle)
// resync the playhead instead of accumulating latency.

import { AudioFrame } from "./protocol";

/** Target jitter buffer — one lost 10 ms block is inaudible, latency stays low. */
const JITTER_S = 0.06;
/** If we fall behind by more than this, resync instead of racing. */
const MAX_LAG_S = 0.25;

export type AudioPreference = { codec: "opus" | "pcm" } | null;

/** Which audio payload this environment can play, or null for "none". */
export function audioPreference(): AudioPreference {
  const hasAudioCtx =
    typeof AudioContext === "function" ||
    typeof (globalThis as { webkitAudioContext?: unknown }).webkitAudioContext === "function";
  if (!hasAudioCtx) return null;
  if (typeof AudioDecoder === "function" && typeof EncodedAudioChunk === "function") {
    return { codec: "opus" };
  }
  return { codec: "pcm" };
}

interface AudioCtxCtor {
  new (opts?: AudioContextOptions): AudioContext;
}

function audioCtxCtor(): AudioCtxCtor {
  return (
    (globalThis as { AudioContext?: AudioCtxCtor }).AudioContext ??
    (globalThis as unknown as { webkitAudioContext: AudioCtxCtor }).webkitAudioContext
  );
}

export class AudioPlayer {
  private ctx: AudioContext;
  private gain: GainNode;
  private decoder: AudioDecoder | null = null;
  private playhead = 0;
  private lastSeq = -1;
  /** Blocks decoded+scheduled (stats/tests). */
  framesPlayed = 0;
  onError: ((e: Error) => void) | null = null;

  constructor(codec: "opus" | "pcm") {
    const Ctor = audioCtxCtor();
    this.ctx = new Ctor({ latencyHint: "interactive", sampleRate: 48_000 });
    this.gain = this.ctx.createGain();
    this.gain.connect(this.ctx.destination);
    if (codec === "opus") this.initDecoder();
  }

  private initDecoder(): void {
    this.decoder = new AudioDecoder({
      output: (data: AudioData) => {
        try {
          this.schedule(audioDataToBuffer(this.ctx, data));
        } finally {
          data.close();
        }
      },
      error: (e: DOMException) => {
        console.error("audio decode error", e);
        this.onError?.(new Error(e.message));
      },
    });
    this.decoder.configure({
      codec: "opus",
      sampleRate: 48_000,
      numberOfChannels: 2,
    });
  }

  /** Volume 0..1 (a GainNode, so it also applies to already-queued audio). */
  setVolume(v: number): void {
    this.gain.gain.value = Math.min(1, Math.max(0, v));
  }

  /** Must be called from a user gesture on some browsers (autoplay policy). */
  async resume(): Promise<void> {
    if (this.ctx.state === "suspended") await this.ctx.resume();
  }

  push(frame: AudioFrame): void {
    if (frame.channels !== 2 || frame.sampleRate !== 48_000) return; // pipeline contract
    // Sequence gap → the stream was interrupted; let the playhead resync.
    if (this.lastSeq >= 0 && frame.seq !== this.lastSeq + 1) this.playhead = 0;
    this.lastSeq = frame.seq;

    if (frame.codec === "opus") {
      if (!this.decoder || this.decoder.state === "closed") return;
      this.decoder.decode(
        new EncodedAudioChunk({
          type: "key",
          timestamp: Number(frame.timestampUs),
          data: frame.payload.slice() as unknown as BufferSource,
        }),
      );
    } else {
      this.schedule(pcmToBuffer(this.ctx, frame.payload));
    }
  }

  private schedule(buf: AudioBuffer): void {
    const now = this.ctx.currentTime;
    if (this.playhead < now + 0.005 || this.playhead > now + MAX_LAG_S) {
      this.playhead = now + JITTER_S; // (re)prime the jitter buffer
    }
    const src = this.ctx.createBufferSource();
    src.buffer = buf;
    src.connect(this.gain);
    src.start(this.playhead);
    this.playhead += buf.duration;
    this.framesPlayed++;
  }

  destroy(): void {
    try {
      this.decoder?.close();
    } catch {
      /* already closed */
    }
    void this.ctx.close();
  }
}

/** WebCodecs AudioData → AudioBuffer via planar f32 copies (universally
 *  supported copy format regardless of the decoder's native layout). */
function audioDataToBuffer(ctx: AudioContext, data: AudioData): AudioBuffer {
  const channels = data.numberOfChannels;
  const buf = ctx.createBuffer(channels, data.numberOfFrames, data.sampleRate);
  const tmp = new Float32Array(data.numberOfFrames);
  for (let c = 0; c < channels; c++) {
    data.copyTo(tmp, { planeIndex: c, format: "f32-planar" });
    buf.getChannelData(c).set(tmp);
  }
  return buf;
}

/** Interleaved s16le stereo → AudioBuffer. */
function pcmToBuffer(ctx: AudioContext, payload: Uint8Array): AudioBuffer {
  const samples = new Int16Array(
    payload.buffer.slice(payload.byteOffset, payload.byteOffset + payload.byteLength),
  );
  const frames = samples.length / 2;
  const buf = ctx.createBuffer(2, frames, 48_000);
  const l = buf.getChannelData(0);
  const r = buf.getChannelData(1);
  for (let i = 0; i < frames; i++) {
    l[i] = samples[i * 2]! / 32768;
    r[i] = samples[i * 2 + 1]! / 32768;
  }
  return buf;
}
