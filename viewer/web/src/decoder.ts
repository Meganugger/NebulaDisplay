// Video decoding: WebCodecs H.264 (Annex B) with JPEG fallback.
// Renders into a canvas; tracks decode timing + queue depth for stats.
//
// Presentation model — "paint immediately, latest frame wins":
// * A decoded frame is painted the moment it exists. The canvas uses
//   `desynchronized: true`, so on supporting compositors the paint bypasses
//   the compositor queue entirely; elsewhere it is picked up by the next
//   compositor deadline — never *later* than a rAF-scheduled paint, and up to
//   a full display frame earlier (the old model parked frames in a slot and
//   painted once per requestAnimationFrame, adding 0–16.7 ms of queueing and
//   stalling completely when rAF throttles in background tabs).
// * If decode output ever outpaces paint (JPEG decode bursts), the single
//   `latest` slot still guarantees only the newest frame is painted.
// * H.264 overload never drops *delta* frames silently (that corrupts every
//   frame until the next IDR — a v0.2 bug). Instead the feeder enters a
//   skip-until-keyframe state and asks the host for a fresh IDR.
//
// Capability notes: WebCodecs only exists in secure contexts, so the
// plain-HTTP LAN deployment always streams JPEG; older iOS Safari lacks
// createImageBitmap, for which we decode through an <img> element instead.

import { caps } from "./caps";
import { VideoFrame as NdspFrame } from "./protocol";

export interface DecoderStats {
  fpsDecoded: number;
  decodeMsAvg: number;
  queueDepth: number;
  framesDropped: number;
  /** capture-timestamp of the most recently presented frame (µs, host clock) */
  lastPresentedTsUs: bigint;
  lastPresentedAtMs: number;
  /** decode-completion → paint delay EMA, ms (presentation scheduling). */
  presentWaitMsAvg: number;
}

/** Decoder queue depth beyond which we resync via keyframe instead of queueing. */
const MAX_DECODE_QUEUE = 8;

type Presentable =
  | { kind: "vf"; frame: VideoFrame; tsUs: bigint; readyAt: number }
  | { kind: "bmp"; frame: ImageBitmap; tsUs: bigint; readyAt: number }
  | { kind: "img"; frame: HTMLImageElement; tsUs: bigint; readyAt: number };

export class Renderer {
  private ctx: CanvasRenderingContext2D;
  private h264: VideoDecoder | null = null;
  private h264Configured = false;
  private decodeTimes: number[] = [];
  private framesInWindow = 0;
  private windowStart = performance.now();
  private pendingTs: { tsUs: bigint; submittedAt: number }[] = [];
  private jpegBusy = false;
  private jpegPending: NdspFrame | null = null;
  // --- latest-frame presentation ---
  private latest: Presentable | null = null;
  private paintScheduled = false;
  private destroyed = false;
  stats: DecoderStats = {
    fpsDecoded: 0,
    decodeMsAvg: 0,
    queueDepth: 0,
    framesDropped: 0,
    lastPresentedTsUs: 0n,
    lastPresentedAtMs: 0,
    presentWaitMsAvg: 0,
  };
  onError: ((e: Error) => void) | null = null;
  /** Ask the host for a keyframe (set by the app; called on decode errors). */
  requestKeyframe: (() => void) | null = null;
  private sawKeyframe = false;
  /** Overload resync: drop everything until the next keyframe arrives. */
  private skipUntilKeyframe = false;

  constructor(private canvas: HTMLCanvasElement) {
    const ctx = canvas.getContext("2d", { desynchronized: true });
    if (!ctx) throw new Error("2d canvas context unavailable");
    this.ctx = ctx;
  }

  async push(frame: NdspFrame): Promise<void> {
    if (frame.codec === "h264") {
      this.pushH264(frame);
    } else if (frame.codec === "jpeg") {
      await this.pushJpeg(frame);
    } else {
      this.onError?.(new Error(`unsupported codec ${frame.codec}`));
    }
  }

  /**
   * Present a frame. Painted synchronously right here — the microtask defer
   * exists only to coalesce decode-output bursts (a queued decoder draining
   * several frames in one task) down to a single paint of the newest one.
   */
  private setLatest(p: Presentable): void {
    if (this.latest) {
      this.closePresentable(this.latest);
      this.stats.framesDropped++;
    }
    this.latest = p;
    if (!this.paintScheduled) {
      this.paintScheduled = true;
      queueMicrotask(() => {
        this.paintScheduled = false;
        if (!this.destroyed) this.paintLatest();
      });
    }
  }

  private closePresentable(p: Presentable): void {
    if (p.kind === "vf") p.frame.close();
    else if (p.kind === "bmp") p.frame.close();
  }

  private paintLatest(): void {
    const p = this.latest;
    if (!p) return;
    this.latest = null;
    let w: number;
    let h: number;
    if (p.kind === "vf") {
      w = p.frame.displayWidth;
      h = p.frame.displayHeight;
    } else if (p.kind === "bmp") {
      w = p.frame.width;
      h = p.frame.height;
    } else {
      w = p.frame.naturalWidth;
      h = p.frame.naturalHeight;
    }
    this.fit(w, h);
    this.ctx.drawImage(p.frame, 0, 0, this.canvas.width, this.canvas.height);
    this.closePresentable(p);
    this.stats.lastPresentedTsUs = p.tsUs;
    const wait = performance.now() - p.readyAt;
    this.stats.presentWaitMsAvg =
      this.stats.presentWaitMsAvg === 0 ? wait : this.stats.presentWaitMsAvg * 0.9 + wait * 0.1;
    this.tickFps();
  }

  private h264ErrorStreak = 0;

  private ensureH264(): VideoDecoder {
    if (this.h264 && this.h264.state !== "closed") return this.h264;
    this.h264 = new VideoDecoder({
      output: (vf: VideoFrame) => this.onDecoded(vf),
      error: (e: DOMException) => {
        console.warn("VideoDecoder error; requesting keyframe", e);
        this.h264Configured = false;
        this.sawKeyframe = false;
        // A keyframe normally recovers a transient error; a *streak* with no
        // presented frame in between means this engine can't decode the
        // stream at all (e.g. codec-less builds) — surface it instead of
        // looping forever on a black canvas.
        this.h264ErrorStreak++;
        if (this.h264ErrorStreak >= 5) {
          this.onError?.(new Error(`H.264 decoding keeps failing: ${e.message}`));
        } else {
          this.requestKeyframe?.();
        }
      },
    });
    return this.h264;
  }

  private pushH264(frame: NdspFrame): void {
    if (!caps.webCodecsH264) {
      // Defensive: the client never advertises h264 without WebCodecs, so a
      // misbehaving host is the only way here — fail clearly, don't crash.
      this.onError?.(new Error("received h264 but WebCodecs is unavailable in this context"));
      return;
    }
    const dec = this.ensureH264();
    if (!this.h264Configured) {
      // Annex B (no description) → decoder parses SPS/PPS from the stream.
      dec.configure({ codec: "avc1.42E01F", optimizeForLatency: true });
      this.h264Configured = true;
    }
    if (this.skipUntilKeyframe && frame.keyframe) {
      this.skipUntilKeyframe = false;
    }
    if (!this.sawKeyframe || this.skipUntilKeyframe) {
      if (!frame.keyframe) {
        this.stats.framesDropped++;
        this.requestKeyframe?.();
        return;
      }
      this.sawKeyframe = true;
    }
    // Overloaded decoder: dropping deltas would corrupt every later frame,
    // so instead resynchronize on a fresh keyframe.
    if (dec.decodeQueueSize > MAX_DECODE_QUEUE && !frame.keyframe) {
      this.skipUntilKeyframe = true;
      this.stats.framesDropped++;
      this.requestKeyframe?.();
      return;
    }
    this.pendingTs.push({ tsUs: frame.timestampUs, submittedAt: performance.now() });
    if (this.pendingTs.length > 120) this.pendingTs.shift();
    dec.decode(
      new EncodedVideoChunk({
        type: frame.keyframe ? "key" : "delta",
        // WebCodecs timestamps are µs; keep the host capture timestamp so we
        // can match output frames back for latency measurement.
        timestamp: Number(frame.timestampUs & 0x7fffffffffffn),
        data: frame.payload as BufferSource,
      }),
    );
    this.stats.queueDepth = dec.decodeQueueSize;
  }

  private onDecoded(vf: VideoFrame): void {
    this.h264ErrorStreak = 0;
    let tsUs = 0n;
    const match = this.pendingTs.find((p) => Number(p.tsUs & 0x7fffffffffffn) === vf.timestamp);
    if (match) {
      this.decodeSample(performance.now() - match.submittedAt);
      tsUs = match.tsUs;
      this.pendingTs = this.pendingTs.filter((p) => p !== match);
    }
    if (this.h264) this.stats.queueDepth = this.h264.decodeQueueSize;
    this.setLatest({ kind: "vf", frame: vf, tsUs, readyAt: performance.now() });
  }

  private async pushJpeg(frame: NdspFrame): Promise<void> {
    // Latest-frame semantics while a decode is in flight: remember the
    // newest arrival (replacing older pendings) and decode it right after.
    if (this.jpegBusy) {
      if (this.jpegPending) this.stats.framesDropped++;
      this.jpegPending = frame;
      return;
    }
    this.jpegBusy = true;
    const t0 = performance.now();
    try {
      const copy = new Uint8Array(frame.payload); // detach from envelope buffer
      const blob = new Blob([copy.buffer as ArrayBuffer], { type: "image/jpeg" });
      if (caps.createImageBitmap) {
        const bmp = await createImageBitmap(blob);
        this.decodeSample(performance.now() - t0);
        this.setLatest({ kind: "bmp", frame: bmp, tsUs: frame.timestampUs, readyAt: performance.now() });
      } else {
        // Older iOS Safari / WebViews: decode through an <img> element.
        const img = await this.decodeViaImage(blob);
        this.decodeSample(performance.now() - t0);
        this.setLatest({ kind: "img", frame: img, tsUs: frame.timestampUs, readyAt: performance.now() });
      }
    } catch (e) {
      this.onError?.(e as Error);
    } finally {
      this.jpegBusy = false;
      const pending = this.jpegPending;
      this.jpegPending = null;
      if (pending) void this.pushJpeg(pending);
    }
  }

  private async decodeViaImage(blob: Blob): Promise<HTMLImageElement> {
    const url = URL.createObjectURL(blob);
    try {
      const img = new Image();
      await new Promise<void>((resolve, reject) => {
        img.onload = () => resolve();
        img.onerror = () => reject(new Error("jpeg decode failed (<img> fallback)"));
        img.src = url;
      });
      return img;
    } finally {
      // Safe to revoke once decode completed; the element keeps its bitmap.
      URL.revokeObjectURL(url);
    }
  }

  private fit(w: number, h: number): void {
    if (this.canvas.width !== w || this.canvas.height !== h) {
      this.canvas.width = w;
      this.canvas.height = h;
    }
  }

  private decodeSample(ms: number): void {
    this.decodeTimes.push(ms);
    if (this.decodeTimes.length > 60) this.decodeTimes.shift();
    this.stats.decodeMsAvg = this.decodeTimes.reduce((a, b) => a + b, 0) / this.decodeTimes.length;
  }

  private tickFps(): void {
    this.framesInWindow++;
    this.stats.lastPresentedAtMs = performance.now();
    const elapsed = performance.now() - this.windowStart;
    if (elapsed >= 1000) {
      this.stats.fpsDecoded = (this.framesInWindow * 1000) / elapsed;
      this.framesInWindow = 0;
      this.windowStart = performance.now();
    }
  }

  destroy(): void {
    this.destroyed = true;
    if (this.latest) {
      this.closePresentable(this.latest);
      this.latest = null;
    }
    if (this.h264 && this.h264.state !== "closed") this.h264.close();
  }
}
