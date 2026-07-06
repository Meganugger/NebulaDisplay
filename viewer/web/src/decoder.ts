// Video decoding: WebCodecs H.264 (Annex B) with JPEG fallback.
// Renders into a canvas; tracks decode timing + queue depth for stats.
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
}

export class Renderer {
  private ctx: CanvasRenderingContext2D;
  private h264: VideoDecoder | null = null;
  private h264Configured = false;
  private decodeTimes: number[] = [];
  private framesInWindow = 0;
  private windowStart = performance.now();
  private pendingTs: { tsUs: bigint; submittedAt: number }[] = [];
  private jpegBusy = false;
  private jpegDropped = 0;
  stats: DecoderStats = {
    fpsDecoded: 0,
    decodeMsAvg: 0,
    queueDepth: 0,
    framesDropped: 0,
    lastPresentedTsUs: 0n,
    lastPresentedAtMs: 0,
  };
  onError: ((e: Error) => void) | null = null;
  /** Ask the host for a keyframe (set by the app; called on decode errors). */
  requestKeyframe: (() => void) | null = null;
  private sawKeyframe = false;

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

  private h264ErrorStreak = 0;

  private ensureH264(): VideoDecoder {
    if (this.h264 && this.h264.state !== "closed") return this.h264;
    this.h264 = new VideoDecoder({
      output: (vf: VideoFrame) => this.present(vf),
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
    if (!this.sawKeyframe) {
      if (!frame.keyframe) {
        this.stats.framesDropped++;
        this.requestKeyframe?.();
        return;
      }
      this.sawKeyframe = true;
    }
    // Shed latency: if the decoder is falling behind, skip delta frames.
    if (dec.decodeQueueSize > 6 && !frame.keyframe) {
      this.stats.framesDropped++;
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

  private present(vf: VideoFrame): void {
    this.h264ErrorStreak = 0;
    const match = this.pendingTs.find((p) => Number(p.tsUs & 0x7fffffffffffn) === vf.timestamp);
    if (match) {
      this.decodeSample(performance.now() - match.submittedAt);
      this.stats.lastPresentedTsUs = match.tsUs;
      this.pendingTs = this.pendingTs.filter((p) => p !== match);
    }
    this.fit(vf.displayWidth, vf.displayHeight);
    this.ctx.drawImage(vf, 0, 0, this.canvas.width, this.canvas.height);
    vf.close();
    this.tickFps();
  }

  private async pushJpeg(frame: NdspFrame): Promise<void> {
    // Drop frames while a decode is in flight — always show the newest.
    if (this.jpegBusy) {
      this.jpegDropped++;
      this.stats.framesDropped++;
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
        this.fit(bmp.width, bmp.height);
        this.ctx.drawImage(bmp, 0, 0, this.canvas.width, this.canvas.height);
        bmp.close();
      } else {
        // Older iOS Safari / WebViews: decode through an <img> element.
        await this.drawViaImage(blob);
        this.decodeSample(performance.now() - t0);
      }
      this.stats.lastPresentedTsUs = frame.timestampUs;
      this.tickFps();
    } catch (e) {
      this.onError?.(e as Error);
    } finally {
      this.jpegBusy = false;
    }
  }

  private async drawViaImage(blob: Blob): Promise<void> {
    const url = URL.createObjectURL(blob);
    try {
      const img = new Image();
      await new Promise<void>((resolve, reject) => {
        img.onload = () => resolve();
        img.onerror = () => reject(new Error("jpeg decode failed (<img> fallback)"));
        img.src = url;
      });
      this.fit(img.naturalWidth, img.naturalHeight);
      this.ctx.drawImage(img, 0, 0, this.canvas.width, this.canvas.height);
    } finally {
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
      this.stats.framesDropped = this.jpegDropped;
    }
  }

  destroy(): void {
    if (this.h264 && this.h264.state !== "closed") this.h264.close();
  }
}
