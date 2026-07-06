// Input capture: pointer/touch/pen/keyboard/wheel → batched NDSP InputEvents.
// Events are batched per animation frame to keep the control channel light.
//
// Capability notes: PointerEvent is missing on older iOS Safari and some
// WebViews, so a touch+mouse event fallback covers those; setPointerCapture
// can throw (InvalidStateError) on some engines and is treated as optional.

import { caps } from "./caps";
import { InputEvent as NdspInput, InputMode, TouchPhase } from "./protocol";

export class InputCapture {
  mode: InputMode = "view_only";
  private batch: NdspInput[] = [];
  private flushScheduled = false;
  private disposers: (() => void)[] = [];

  constructor(
    private surface: HTMLElement,
    private send: (events: NdspInput[]) => void,
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
        this.push({ kind: "wheel", dx: ev.deltaX / 100, dy: ev.deltaY / 100 });
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
    on(s, "pointermove", (ev: PointerEvent) => this.pointer(ev, "move"));
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
  }

  private norm(clientX: number, clientY: number): { x: number; y: number } {
    const r = this.surface.getBoundingClientRect();
    return {
      x: Math.min(1, Math.max(0, (clientX - r.left) / r.width)),
      y: Math.min(1, Math.max(0, (clientY - r.top) / r.height)),
    };
  }

  private pointer(ev: PointerEvent, phase: TouchPhase): void {
    if (this.mode === "view_only") return;
    const { x, y } = this.norm(ev.clientX, ev.clientY);
    if (ev.pointerType === "touch") {
      this.push({ kind: "touch", id: ev.pointerId >>> 0, phase, x, y, pressure: ev.pressure });
      ev.preventDefault();
      return;
    }
    if (ev.pointerType === "pen" || this.mode === "drawing_tablet") {
      this.push({
        kind: "pen",
        phase,
        x,
        y,
        pressure: ev.pressure,
        tilt_x: ev.tiltX / 90,
        tilt_y: ev.tiltY / 90,
      });
      ev.preventDefault();
      return;
    }
    this.mouseAt(x, y, phase, ev.button);
  }

  private touch(ev: TouchEvent, phase: TouchPhase): void {
    if (this.mode === "view_only") return;
    ev.preventDefault();
    for (let i = 0; i < ev.changedTouches.length; i++) {
      const t = ev.changedTouches[i]!;
      const { x, y } = this.norm(t.clientX, t.clientY);
      const ended = phase === "end" || phase === "cancel";
      // Touch.force is 0 on hardware without pressure — report full contact.
      const pressure = ended ? 0 : t.force > 0 ? t.force : 1;
      this.push({ kind: "touch", id: t.identifier >>> 0, phase, x, y, pressure });
    }
  }

  private mouse(ev: MouseEvent, phase: TouchPhase): void {
    if (this.mode === "view_only") return;
    const { x, y } = this.norm(ev.clientX, ev.clientY);
    this.mouseAt(x, y, phase, ev.button);
  }

  private mouseAt(x: number, y: number, phase: TouchPhase, button: number): void {
    if (phase === "move") {
      this.push({ kind: "mouse_move", x, y });
    } else if (phase === "start" || phase === "end") {
      this.push({ kind: "mouse_move", x, y });
      this.push({ kind: "mouse_button", button: mapButton(button), pressed: phase === "start" });
    }
  }

  private key(ev: KeyboardEvent, pressed: boolean): void {
    if (this.mode === "view_only" || this.mode === "touchpad" || this.mode === "direct_touch")
      return;
    // Keep browser shortcuts like F11/F12 & Escape local.
    if (["F11", "F12", "Escape"].includes(ev.code)) return;
    ev.preventDefault();
    this.push({ kind: "key", code: ev.code, pressed });
  }

  private push(e: NdspInput): void {
    this.batch.push(e);
    if (!this.flushScheduled) {
      this.flushScheduled = true;
      requestAnimationFrame(() => {
        this.flushScheduled = false;
        if (this.batch.length > 0) {
          const b = this.batch;
          this.batch = [];
          this.send(b);
        }
      });
    }
  }
}

function mapButton(domButton: number): number {
  // DOM: 0=left 1=middle 2=right 3=back 4=forward → NDSP uses the same order.
  return domButton;
}
