// Input capture: pointer/touch/pen/keyboard/wheel → batched NDSP InputEvents.
// Events are batched per animation frame to keep the control channel light.

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
    const s = this.surface;
    const on = <K extends keyof HTMLElementEventMap>(
      target: HTMLElement | Window,
      type: K | string,
      fn: (ev: never) => void,
      opts?: AddEventListenerOptions,
    ) => {
      target.addEventListener(type as string, fn as EventListener, opts);
      this.disposers.push(() => target.removeEventListener(type as string, fn as EventListener));
    };

    on(s, "pointermove", (ev: PointerEvent) => this.pointer(ev, "move"));
    on(s, "pointerdown", (ev: PointerEvent) => {
      s.setPointerCapture(ev.pointerId);
      this.pointer(ev, "start");
    });
    on(s, "pointerup", (ev: PointerEvent) => this.pointer(ev, "end"));
    on(s, "pointercancel", (ev: PointerEvent) => this.pointer(ev, "cancel"));
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

  detach(): void {
    for (const d of this.disposers) d();
    this.disposers = [];
  }

  private norm(ev: PointerEvent): { x: number; y: number } {
    const r = this.surface.getBoundingClientRect();
    return {
      x: Math.min(1, Math.max(0, (ev.clientX - r.left) / r.width)),
      y: Math.min(1, Math.max(0, (ev.clientY - r.top) / r.height)),
    };
  }

  private pointer(ev: PointerEvent, phase: TouchPhase): void {
    if (this.mode === "view_only") return;
    const { x, y } = this.norm(ev);
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
    // Mouse.
    if (phase === "move") {
      this.push({ kind: "mouse_move", x, y });
    } else if (phase === "start" || phase === "end") {
      this.push({ kind: "mouse_move", x, y });
      this.push({ kind: "mouse_button", button: mapButton(ev.button), pressed: phase === "start" });
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
