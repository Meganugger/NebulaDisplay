// Input capture: pointer/touch/pen/keyboard/wheel → NDSP InputEvents.
//
// Latency design:
// * Discrete events (down/up/wheel/key) are sent IMMEDIATELY — never batched.
// * Move events are coalesced over at most MOVE_FLUSH_MS (4 ms), preserving
//   every sample via getCoalescedEvents, then sent as one batch. The v0.2
//   design batched per requestAnimationFrame, which added up to a display
//   frame of latency and stalled entirely whenever rAF throttled — the root
//   cause of "touch teleports" and broken dragging.
// * `pointerrawupdate` is used where available so movement is sampled at
//   device rate (120–240 Hz) instead of display rate.
//
// Coordinate mapping: the canvas element fills the screen with
// `object-fit: contain`, so the actual video content sits inside a
// letterboxed content box. Coordinates are normalized against that content
// box (not the element box) — see `mapToContent` — so every tap lands on the
// exact intended desktop pixel regardless of aspect ratio, DPR or rotation.
//
// Capability notes: PointerEvent is missing on older iOS Safari and some
// WebViews, so a touch+mouse event fallback covers those; setPointerCapture
// can throw (InvalidStateError) on some engines and is treated as optional.

import { caps } from "./caps";
import { InputEvent as NdspInput, InputMode, TouchPhase } from "./protocol";

/** Maximum time a move sample may wait for coalescing before being sent. */
const MOVE_FLUSH_MS = 4;

/**
 * Map a client-space point to 0..1 coordinates of the *displayed video
 * content* inside an `object-fit: contain` element. Pure function (unit
 * tested in tests/web-compat.mjs).
 */
/**
 * The displayed content box (screen pixels) of an `object-fit: contain`
 * element: where the video actually is, inside the letterboxing.
 */
export function contentBox(
  rect: { left: number; top: number; width: number; height: number },
  contentW: number,
  contentH: number,
): { left: number; top: number; width: number; height: number; scale: number } {
  if (contentW <= 0 || contentH <= 0 || rect.width <= 0 || rect.height <= 0) {
    return { left: rect.left, top: rect.top, width: rect.width, height: rect.height, scale: 1 };
  }
  const scale = Math.min(rect.width / contentW, rect.height / contentH);
  const width = contentW * scale;
  const height = contentH * scale;
  return {
    left: rect.left + (rect.width - width) / 2,
    top: rect.top + (rect.height - height) / 2,
    width,
    height,
    scale,
  };
}

export function mapToContent(
  rect: { left: number; top: number; width: number; height: number },
  contentW: number,
  contentH: number,
  clientX: number,
  clientY: number,
): { x: number; y: number } {
  let boxW = rect.width;
  let boxH = rect.height;
  let offX = rect.left;
  let offY = rect.top;
  if (contentW > 0 && contentH > 0 && rect.width > 0 && rect.height > 0) {
    const scale = Math.min(rect.width / contentW, rect.height / contentH);
    boxW = contentW * scale;
    boxH = contentH * scale;
    offX = rect.left + (rect.width - boxW) / 2;
    offY = rect.top + (rect.height - boxH) / 2;
  }
  return {
    x: Math.min(1, Math.max(0, (clientX - offX) / boxW)),
    y: Math.min(1, Math.max(0, (clientY - offY) / boxH)),
  };
}

export class InputCapture {
  mode: InputMode = "view_only";
  private batch: NdspInput[] = [];
  private flushTimer: number | undefined;
  private lastFlushAt = 0;
  private disposers: (() => void)[] = [];

  constructor(
    private surface: HTMLElement,
    private send: (events: NdspInput[]) => void,
    /** Native size of the displayed video (for letterbox mapping). */
    private contentSize: () => { w: number; h: number },
  ) {}

  attach(): void {
    const on = <K extends keyof HTMLElementEventMap>(
      target: HTMLElement | Window,
      type: K | string,
      fn: (ev: never) => void,
      opts?: AddEventListenerOptions,
    ) => {
      target.addEventListener(type as string, fn as EventListener, opts);
      this.disposers.push(() => target.removeEventListener(type as string, fn as EventListener));
    };

    if (caps.pointerEvents) {
      this.attachPointer(on);
    } else {
      this.attachTouchMouse(on);
    }

    const s = this.surface;
    on(
      s,
      "wheel",
      (ev: WheelEvent) => {
        if (this.mode === "view_only") return;
        ev.preventDefault();
        // deltaMode: 0=pixels (~100/notch), 1=lines (~3/notch), 2=pages.
        const k = ev.deltaMode === 1 ? 1 / 3 : ev.deltaMode === 2 ? 1 : 1 / 100;
        this.push({ kind: "wheel", dx: ev.deltaX * k, dy: ev.deltaY * k }, true);
      },
      { passive: false },
    );
    on(s, "contextmenu", (ev: MouseEvent) => {
      if (this.mode !== "view_only") ev.preventDefault();
    });
    on(window, "keydown", (ev: KeyboardEvent) => this.key(ev, true));
    on(window, "keyup", (ev: KeyboardEvent) => this.key(ev, false));
  }

  private attachPointer(
    on: (
      target: HTMLElement | Window,
      type: string,
      fn: (ev: never) => void,
      opts?: AddEventListenerOptions,
    ) => void,
  ): void {
    const s = this.surface;
    // Moves: device-rate samples where the engine offers them. When
    // pointerrawupdate exists, pointermove would deliver duplicate (already
    // coalesced) samples — so only one of the two is wired.
    if (caps.pointerRawUpdate) {
      on(s, "pointerrawupdate", (ev: PointerEvent) => this.pointerMove(ev));
    } else {
      on(s, "pointermove", (ev: PointerEvent) => this.pointerMove(ev));
    }
    on(s, "pointerdown", (ev: PointerEvent) => {
      try {
        s.setPointerCapture?.(ev.pointerId);
      } catch {
        /* capture is best-effort; some engines throw InvalidStateError */
      }
      this.pointer(ev, "start");
    });
    on(s, "pointerup", (ev: PointerEvent) => this.pointer(ev, "end"));
    on(s, "pointercancel", (ev: PointerEvent) => this.pointer(ev, "cancel"));
  }

  /** Fallback for engines without PointerEvent: raw touch + mouse events. */
  private attachTouchMouse(
    on: (
      target: HTMLElement | Window,
      type: string,
      fn: (ev: never) => void,
      opts?: AddEventListenerOptions,
    ) => void,
  ): void {
    const s = this.surface;
    const touchOpts: AddEventListenerOptions = { passive: false };
    on(s, "touchstart", (ev: TouchEvent) => this.touch(ev, "start"), touchOpts);
    on(s, "touchmove", (ev: TouchEvent) => this.touch(ev, "move"), touchOpts);
    on(s, "touchend", (ev: TouchEvent) => this.touch(ev, "end"), touchOpts);
    on(s, "touchcancel", (ev: TouchEvent) => this.touch(ev, "cancel"), touchOpts);
    on(s, "mousemove", (ev: MouseEvent) => this.mouse(ev, "move"));
    on(s, "mousedown", (ev: MouseEvent) => this.mouse(ev, "start"));
    // Listen on window so a release outside the canvas still ends the drag.
    on(window, "mouseup", (ev: MouseEvent) => this.mouse(ev, "end"));
  }

  detach(): void {
    for (const d of this.disposers) d();
    this.disposers = [];
    if (this.flushTimer !== undefined) {
      clearTimeout(this.flushTimer);
      this.flushTimer = undefined;
    }
    this.batch = [];
  }

  private norm(clientX: number, clientY: number): { x: number; y: number } {
    const r = this.surface.getBoundingClientRect();
    const { w, h } = this.contentSize();
    return mapToContent(
      { left: r.left, top: r.top, width: r.width, height: r.height },
      w,
      h,
      clientX,
      clientY,
    );
  }

  /** Move samples, expanded to every coalesced (device-rate) sample. */
  private pointerMove(ev: PointerEvent): void {
    if (this.mode === "view_only") return;
    const samples: PointerEvent[] = caps.coalescedEvents
      ? ((ev.getCoalescedEvents() as PointerEvent[] | undefined) ?? [ev])
      : [ev];
    for (const s of samples.length > 0 ? samples : [ev]) {
      this.emitPointer(s, "move");
    }
    if (ev.cancelable) ev.preventDefault();
  }

  private pointer(ev: PointerEvent, phase: TouchPhase): void {
    if (this.mode === "view_only") return;
    this.emitPointer(ev, phase);
    if (ev.cancelable) ev.preventDefault();
  }

  private emitPointer(ev: PointerEvent, phase: TouchPhase): void {
    const { x, y } = this.norm(ev.clientX, ev.clientY);
    const discrete = phase !== "move";
    if (ev.pointerType === "touch") {
      this.push(
        { kind: "touch", id: ev.pointerId >>> 0, phase, x, y, pressure: ev.pressure },
        discrete,
      );
      return;
    }
    if (ev.pointerType === "pen" || this.mode === "drawing_tablet") {
      this.push(
        {
          kind: "pen",
          phase,
          x,
          y,
          pressure: ev.pressure,
          tilt_x: ev.tiltX / 90,
          tilt_y: ev.tiltY / 90,
        },
        discrete,
      );
      return;
    }
    this.mouseAt(x, y, phase, ev.button);
  }

  private touch(ev: TouchEvent, phase: TouchPhase): void {
    if (this.mode === "view_only") return;
    ev.preventDefault();
    const discrete = phase !== "move";
    for (let i = 0; i < ev.changedTouches.length; i++) {
      const t = ev.changedTouches[i]!;
      const { x, y } = this.norm(t.clientX, t.clientY);
      const ended = phase === "end" || phase === "cancel";
      // Touch.force is 0 on hardware without pressure — report full contact.
      const pressure = ended ? 0 : t.force > 0 ? t.force : 1;
      this.push({ kind: "touch", id: t.identifier >>> 0, phase, x, y, pressure }, discrete);
    }
  }

  private mouse(ev: MouseEvent, phase: TouchPhase): void {
    if (this.mode === "view_only") return;
    const { x, y } = this.norm(ev.clientX, ev.clientY);
    this.mouseAt(x, y, phase, ev.button);
  }

  private mouseAt(x: number, y: number, phase: TouchPhase, button: number): void {
    if (phase === "move") {
      this.push({ kind: "mouse_move", x, y }, false);
    } else if (phase === "start" || phase === "end") {
      this.push({ kind: "mouse_move", x, y }, false);
      this.push(
        { kind: "mouse_button", button: mapButton(button), pressed: phase === "start" },
        true,
      );
    }
  }

  private key(ev: KeyboardEvent, pressed: boolean): void {
    if (this.mode === "view_only" || this.mode === "touchpad" || this.mode === "direct_touch")
      return;
    // Keep browser shortcuts like F11/F12 & Escape local.
    if (["F11", "F12", "Escape"].includes(ev.code)) return;
    ev.preventDefault();
    // Layout hint: the character this key produced under *this* keyboard
    // layout. The host resolves it against its own layout so typing stays
    // correct across layout mismatches (docs/PROTOCOL.md, roadmap item 13).
    const key = ev.key.length === 1 ? ev.key : undefined;
    this.push(key ? { kind: "key", code: ev.code, pressed, key } : { kind: "key", code: ev.code, pressed }, true);
  }

  /**
   * Queue an event. `urgent` events (button/key/wheel/phase changes) flush
   * the whole batch immediately, preserving order. Move samples flush at
   * once if the last flush was ≥ MOVE_FLUSH_MS ago, else after the remainder
   * of that window — bounding added latency to 4 ms while capping message
   * rate at 250/s no matter how fast the input device samples.
   */
  private push(e: NdspInput, urgent: boolean): void {
    this.batch.push(e);
    const now = performance.now();
    if (urgent || now - this.lastFlushAt >= MOVE_FLUSH_MS) {
      this.flush(now);
      return;
    }
    if (this.flushTimer === undefined) {
      const wait = Math.max(0, MOVE_FLUSH_MS - (now - this.lastFlushAt));
      this.flushTimer = window.setTimeout(() => {
        this.flushTimer = undefined;
        this.flush(performance.now());
      }, wait);
    }
  }

  private flush(now: number): void {
    if (this.flushTimer !== undefined) {
      clearTimeout(this.flushTimer);
      this.flushTimer = undefined;
    }
    if (this.batch.length === 0) return;
    this.lastFlushAt = now;
    const b = this.batch;
    this.batch = [];
    this.send(b);
  }
}

function mapButton(domButton: number): number {
  // DOM: 0=left 1=middle 2=right 3=back 4=forward → NDSP uses the same order.
  return domButton;
}
