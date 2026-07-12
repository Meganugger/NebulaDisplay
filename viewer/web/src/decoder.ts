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
import { Fmp4Muxer } from "./fmp4";
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
    if (frame.codec === "h264" || frame.codec === "hevc") {
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

  // ---- MSE H.264 path (insecure contexts: no WebCodecs, MSE works) --------
  private mse: MseSink | null = null;

  private pushH264ViaMse(frame: NdspFrame): void {
    if (!this.mse) {
      this.mse = new MseSink(
        (video, tsUs) => this.onMseFrame(video, tsUs),
        (e) => this.onError?.(e),
        () => this.requestKeyframe?.(),
      );
    }
    if (!this.sawKeyframe) {
      if (!frame.keyframe) {
        this.stats.framesDropped++;
        this.requestKeyframe?.();
        return;
      }
      this.sawKeyframe = true;
    }
    const t0 = performance.now();
    this.mse.push(frame);
    this.stats.queueDepth = this.mse.queueDepth;
    // MSE decode time isn't observable per frame; charge the mux+append.
    this.decodeSample(performance.now() - t0);
  }

  private onMseFrame(video: HTMLVideoElement, tsUs: bigint): void {
    const w = video.videoWidth;
    const h = video.videoHeight;
    if (w === 0 || h === 0) return;
    this.fit(w, h);
    this.ctx.drawImage(video, 0, 0, w, h);
    this.stats.lastPresentedTsUs = tsUs;
    this.tickFps();
  }

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
      // The MSE remux fallback is H.264-only (no hevc on insecure origins —
      // the client never advertises it there).
      if (caps.mseH264 && frame.codec === "h264") {
        this.pushH264ViaMse(frame);
        return;
      }
      // Defensive: the client never advertises a codec without a decoder, so
      // a misbehaving host is the only way here — fail clearly, don't crash.
      this.onError?.(new Error(`received ${frame.codec} but no decoder in this context`));
      return;
    }
    const dec = this.ensureH264();
    if (!this.h264Configured) {
      // Annex B (no description) → decoder parses parameter sets in-stream.
      const codec = frame.codec === "hevc" ? "hev1.1.6.L120.90" : "avc1.42E01F";
      dec.configure({ codec, optimizeForLatency: true });
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
    this.mse?.destroy();
    this.mse = null;
  }
}

// ---------------------------------------------------------------------------
// MSE sink: hidden <video> + MediaSource fed single-frame fMP4 fragments.
// ---------------------------------------------------------------------------

/** SourceBuffer backlog beyond which we resync via keyframe. */
const MSE_MAX_QUEUE = 8;
/** Chase threshold: how far behind live playback may drift (seconds). */
const MSE_MAX_BEHIND_S = 0.12;

class MseSink {
  private video: HTMLVideoElement;
  private muxer = new Fmp4Muxer();
  private ms: MediaSource | null = null;
  private sb: SourceBuffer | null = null;
  private appendQueue: Uint8Array[] = [];
  private opened = false;
  private skipUntilKeyframe = false;
  private destroyed = false;
  /** capture timestamps of frames in flight, matched FIFO on presentation. */
  private tsFifo: bigint[] = [];
  private rvfcId = 0;
  private rafId = 0;

  constructor(
    private onFrame: (video: HTMLVideoElement, tsUs: bigint) => void,
    private onError: (e: Error) => void,
    private requestKeyframe: () => void,
  ) {
    const v = document.createElement("video");
    v.muted = true;
    v.autoplay = true;
    v.playsInline = true;
    // Kept off-DOM: we only sample it into the canvas.
    v.style.display = "none";
    document.body.appendChild(v);
    this.video = v;
    this.schedulePresent();
  }

  get queueDepth(): number {
    return this.appendQueue.length + this.tsFifo.length;
  }

  push(frame: NdspFrame): void {
    if (this.skipUntilKeyframe) {
      if (!frame.keyframe) {
        this.requestKeyframe();
        return;
      }
      this.skipUntilKeyframe = false;
    }
    if (this.appendQueue.length > MSE_MAX_QUEUE) {
      // SourceBuffer can't keep up — drop until the next keyframe.
      this.skipUntilKeyframe = true;
      this.appendQueue = [];
      this.tsFifo = [];
      this.requestKeyframe();
      return;
    }
    const segments = this.muxer.push(
      frame.payload,
      frame.keyframe,
      frame.timestampUs,
      frame.width,
      frame.height,
    );
    if (!segments) return;
    if (!this.ms) this.openMediaSource();
    this.tsFifo.push(frame.timestampUs);
    if (this.tsFifo.length > 240) this.tsFifo.shift();
    for (const seg of segments) this.appendQueue.push(seg);
    this.pump();
  }

  private openMediaSource(): void {
    const ms = new MediaSource();
    this.ms = ms;
    this.video.src = URL.createObjectURL(ms);
    ms.addEventListener("sourceopen", () => {
      if (this.destroyed) return;
      try {
        const sb = ms.addSourceBuffer(this.muxer.codecString());
        sb.mode = "segments";
        sb.addEventListener("updateend", () => this.pump());
        sb.addEventListener("error", () => this.onError(new Error("MSE SourceBuffer error")));
        this.sb = sb;
        this.opened = true;
        this.pump();
        void this.video.play().catch(() => {
          /* autoplay policies don't apply to muted video, but be safe */
        });
      } catch (e) {
        this.onError(e as Error);
      }
    });
  }

  private pump(): void {
    const sb = this.sb;
    if (!sb || !this.opened || sb.updating || this.destroyed) return;
    const seg = this.appendQueue.shift();
    if (!seg) return;
    try {
      sb.appendBuffer(seg.slice().buffer as ArrayBuffer);
    } catch (e) {
      // QuotaExceeded or state error: trim old buffer + resync.
      try {
        const buffered = sb.buffered;
        if (buffered.length > 0) {
          sb.remove(0, Math.max(0, this.video.currentTime - 1));
        }
      } catch {
        /* ignore */
      }
      this.skipUntilKeyframe = true;
      this.appendQueue = [];
      this.requestKeyframe();
      void e;
    }
  }

  /** Paint per presented frame; chase the live edge to bound latency. */
  private schedulePresent(): void {
    const step = (): void => {
      if (this.destroyed) return;
      const v = this.video;
      // Live-edge chase: if playback drifted behind the buffered end (tab
      // was hidden, decoder hiccup), jump close to the newest frame.
      try {
        const buffered = v.buffered;
        if (buffered.length > 0) {
          const end = buffered.end(buffered.length - 1);
          if (end - v.currentTime > MSE_MAX_BEHIND_S) {
            v.currentTime = Math.max(0, end - 0.01);
            // Frames between old and new position were skipped: keep only
            // the newest capture timestamp so latency stays honestly matched.
            if (this.tsFifo.length > 1) this.tsFifo.splice(0, this.tsFifo.length - 1);
          }
        }
      } catch {
        /* buffered can throw during teardown */
      }
      if (v.readyState >= 2 && v.videoWidth > 0) {
        const ts = this.tsFifo.shift() ?? 0n;
        this.onFrame(v, ts);
      }
      this.scheduleNext(step);
    };
    this.scheduleNext(step);
  }

  private scheduleNext(step: () => void): void {
    if (caps.rvfc) {
      this.rvfcId = this.video.requestVideoFrameCallback(() => step());
    } else {
      this.rafId = requestAnimationFrame(() => step());
    }
  }

  destroy(): void {
    this.destroyed = true;
    if (caps.rvfc && this.rvfcId) this.video.cancelVideoFrameCallback(this.rvfcId);
    if (this.rafId) cancelAnimationFrame(this.rafId);
    try {
      this.video.pause();
      this.video.removeAttribute("src");
      this.video.load();
    } catch {
      /* ignore */
    }
    this.video.remove();
    this.ms = null;
    this.sb = null;
  }
}
