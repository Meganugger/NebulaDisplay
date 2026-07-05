/**
 * Viewer page logic: connect UI, input capture (mouse/touch/stylus/keyboard),
 * stats overlay, fullscreen, profiles.
 */

import { NebulaClient } from "./client";
import { InputEvent, Profile, StreamStats } from "./protocol";

const $ = <T extends HTMLElement>(id: string): T => {
  const el = document.getElementById(id);
  if (!el) throw new Error(`missing element #${id}`);
  return el as T;
};

const canvas = $<HTMLCanvasElement>("screen");
const overlay = $("connect-overlay");
const connectForm = $("connect-form");
const pinForm = $("pin-form");
const statusLine = $("connect-status");
const hostInput = $<HTMLInputElement>("host-input");
const pinInput = $<HTMLInputElement>("pin-input");
const pinError = $("pin-error");
const toolbar = $("toolbar");
const statsOverlay = $("stats-overlay");
const inputBanner = $("input-banner");
const inputModeSel = $<HTMLSelectElement>("input-mode");
const profileLive = $<HTMLSelectElement>("profile-live");

const client = new NebulaClient(canvas);

type InputMode = "view" | "direct" | "touchpad" | "draw";
let inputMode: InputMode = "direct";

// ---------------------------------------------------------------------------
// Connect flow
// ---------------------------------------------------------------------------

// Pre-fill host: URL fragment (QR flow) > same-origin (page served by host).
const frag = new URLSearchParams(location.hash.slice(1));
hostInput.value = frag.get("host") ?? location.host;
const fragPin = frag.get("pin");

function wsUrlFor(hostPort: string): string {
  const scheme = location.protocol === "https:" ? "wss" : "ws";
  return `${scheme}://${hostPort}/ws`;
}

$("connect-btn").addEventListener("click", startConnect);
hostInput.addEventListener("keydown", (e) => e.key === "Enter" && startConnect());

function startConnect(): void {
  const host = hostInput.value.trim();
  if (!host) return;
  const profile = $<HTMLSelectElement>("profile-select").value as Profile;
  profileLive.value = profile;
  client.connect(wsUrlFor(host), {
    profile,
    wantAudio: $<HTMLInputElement>("audio-check").checked,
  });
}

$("pin-btn").addEventListener("click", submitPin);
pinInput.addEventListener("keydown", (e) => e.key === "Enter" && submitPin());

function submitPin(): void {
  const pin = pinInput.value.trim();
  if (pin.length === 6) {
    pinError.hidden = true;
    client.pair(pin);
  }
}

client.on("state", (s, detail) => {
  switch (s) {
    case "connecting":
      statusLine.textContent = detail ?? "Connecting…";
      overlay.hidden = false;
      toolbar.hidden = true;
      break;
    case "hello":
      statusLine.textContent = "Negotiating…";
      break;
    case "need_pairing":
      overlay.hidden = false;
      connectForm.hidden = true;
      pinForm.hidden = false;
      pinInput.focus();
      if (fragPin && pinInput.value === "") {
        pinInput.value = fragPin;
        submitPin();
      }
      break;
    case "authenticating":
      statusLine.textContent = "Authenticating…";
      break;
    case "ready":
      statusLine.textContent = detail ? `Session ended: ${detail}` : "Starting stream…";
      break;
    case "streaming":
      overlay.hidden = true;
      pinForm.hidden = true;
      connectForm.hidden = false;
      toolbar.hidden = false;
      canvas.focus();
      break;
    case "disconnected":
      overlay.hidden = false;
      toolbar.hidden = true;
      statusLine.textContent = "Disconnected.";
      break;
  }
});

client.on("error", (code, message) => {
  if (code === "bad_pin") {
    pinError.textContent = "Wrong PIN — check the host control panel.";
    pinError.hidden = false;
  } else if (code === "pin_expired") {
    pinError.textContent = "PIN expired — generate a new one on the host.";
    pinError.hidden = false;
  } else {
    statusLine.textContent = `Error: ${message}`;
  }
});

client.on("sessionInfo", (w, h) => {
  $("host-label").textContent = `${hostInput.value}  ·  ${w}×${h}`;
});

client.on("inputPermission", (allowed) => {
  inputBanner.hidden = allowed || inputMode === "view";
});

// ---------------------------------------------------------------------------
// Toolbar
// ---------------------------------------------------------------------------

inputModeSel.addEventListener("change", () => {
  inputMode = inputModeSel.value as InputMode;
  inputBanner.hidden = client.inputAllowed || inputMode === "view";
});

profileLive.addEventListener("change", () => client.setProfile(profileLive.value as Profile));

$("disconnect-btn").addEventListener("click", () => client.disconnect());
$("fullscreen-btn").addEventListener("click", () => {
  if (document.fullscreenElement) void document.exitFullscreen();
  else void document.documentElement.requestFullscreen();
});

let statsVisible = false;
$("stats-btn").addEventListener("click", () => {
  statsVisible = !statsVisible;
  statsOverlay.hidden = !statsVisible;
});

client.on("stats", (host: StreamStats, mine) => {
  if (!statsVisible) return;
  statsOverlay.textContent =
    `stream   ${host.width}×${host.height} q${host.quality}\n` +
    `host fps ${host.fps.toFixed(1)}   sent ${host.frames_sent}  drop ${host.frames_dropped}\n` +
    `my fps   ${mine.fpsPresented.toFixed(1)}   decode ${mine.decodeMs.toFixed(1)}ms\n` +
    `bitrate  ${(mine.bitrateKbps / 1000).toFixed(2)} Mbps\n` +
    `rtt      ${mine.rttMs.toFixed(1)} ms   e2e≈${(mine.rttMs / 2 + mine.decodeMs + host.encode_ms + host.capture_ms).toFixed(0)} ms\n` +
    `encode   ${host.encode_ms.toFixed(1)}ms  capture ${host.capture_ms.toFixed(1)}ms`;
});

// ---------------------------------------------------------------------------
// Input capture
// ---------------------------------------------------------------------------

/** Map a pointer event to normalized stream coordinates (letterboxed). */
function normCoords(e: PointerEvent | WheelEvent): { x: number; y: number } | null {
  const rect = canvas.getBoundingClientRect();
  // object-fit: contain → compute the letterboxed content box.
  const scale = Math.min(rect.width / canvas.width, rect.height / canvas.height);
  const cw = canvas.width * scale;
  const ch = canvas.height * scale;
  const ox = rect.left + (rect.width - cw) / 2;
  const oy = rect.top + (rect.height - ch) / 2;
  const x = (e.clientX - ox) / cw;
  const y = (e.clientY - oy) / ch;
  if (x < 0 || x > 1 || y < 0 || y > 1) return null;
  return { x, y };
}

// Touchpad mode state: a client-side virtual cursor.
let padX = 0.5;
let padY = 0.5;
let lastPad: { x: number; y: number } | null = null;

function buttonName(b: number): "left" | "right" | "middle" | "back" | "forward" {
  return (["left", "middle", "right", "back", "forward"] as const)[b] ?? "left";
}

canvas.addEventListener("pointerdown", (e) => {
  canvas.focus();
  canvas.setPointerCapture(e.pointerId);
  handlePointer(e, "down");
  e.preventDefault();
});
canvas.addEventListener("pointermove", (e) => handlePointer(e, "move"));
canvas.addEventListener("pointerup", (e) => handlePointer(e, "up"));
canvas.addEventListener("pointercancel", (e) => handlePointer(e, "cancel"));
canvas.addEventListener("contextmenu", (e) => e.preventDefault());

function handlePointer(e: PointerEvent, phase: "down" | "move" | "up" | "cancel"): void {
  if (inputMode === "view") return;
  const pos = normCoords(e);

  if (e.pointerType === "pen" && (inputMode === "draw" || inputMode === "direct")) {
    if (!pos) return;
    client.sendInput({
      kind: "stylus",
      x: pos.x,
      y: pos.y,
      pressure: e.pressure,
      tilt_x: e.tiltX / 90,
      tilt_y: e.tiltY / 90,
      down: phase === "down" || (phase === "move" && e.buttons > 0),
      eraser: false,
    });
    return;
  }

  if (e.pointerType === "touch") {
    if (inputMode === "touchpad") {
      // Relative movement drives a virtual cursor; tap = click.
      if (phase === "down") lastPad = pos;
      else if (phase === "move" && pos && lastPad) {
        padX = Math.min(1, Math.max(0, padX + (pos.x - lastPad.x) * 1.4));
        padY = Math.min(1, Math.max(0, padY + (pos.y - lastPad.y) * 1.4));
        lastPad = pos;
        client.sendInput({ kind: "mouse_move", x: padX, y: padY });
      } else if (phase === "up") {
        lastPad = null;
        client.sendInput({ kind: "mouse_button", button: "left", down: true, x: padX, y: padY });
        client.sendInput({ kind: "mouse_button", button: "left", down: false, x: padX, y: padY });
      }
      return;
    }
    if (!pos) return;
    client.sendInput({
      kind: "touch",
      id: e.pointerId >>> 0,
      phase,
      x: pos.x,
      y: pos.y,
      pressure: e.pressure > 0 ? e.pressure : null,
    });
    return;
  }

  // Mouse.
  if (!pos) return;
  if (phase === "move") {
    client.sendInput({ kind: "mouse_move", x: pos.x, y: pos.y });
  } else if (phase === "down" || phase === "up") {
    client.sendInput({
      kind: "mouse_button",
      button: buttonName(e.button),
      down: phase === "down",
      x: pos.x,
      y: pos.y,
    });
  }
}

canvas.addEventListener(
  "wheel",
  (e) => {
    if (inputMode === "view") return;
    client.sendInput({ kind: "mouse_wheel", dx: e.deltaX / 100, dy: e.deltaY / 100 });
    e.preventDefault();
  },
  { passive: false },
);

window.addEventListener("keydown", (e) => sendKey(e, true));
window.addEventListener("keyup", (e) => sendKey(e, false));

function sendKey(e: KeyboardEvent, down: boolean): void {
  if (inputMode === "view" || client.getState() !== "streaming") return;
  if (!overlay.hidden) return; // typing into the connect form
  // Keep browser shortcuts working when not fullscreen? No — the remote
  // desktop owns the keyboard while streaming, except F11.
  if (e.code === "F11") return;
  const ev: InputEvent = { kind: "key", code: e.code, down };
  client.sendInput(ev);
  e.preventDefault();
}

// Auto-connect when served from the host itself or launched via QR link.
if (frag.get("autoconnect") === "1" || location.pathname.startsWith("/view")) {
  startConnect();
}
