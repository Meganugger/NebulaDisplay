// Viewer application: connect card → streaming canvas with toolbar/stats.

import "./style.css";
import { capabilityReport, caps, fullscreen, storage } from "./caps";
import { ClockSync } from "./clock";
import { loadCredentials } from "./crypto";
import { usingNativeCrypto } from "./cryptobox";
import { Renderer } from "./decoder";
import { InputCapture } from "./input";
import { ControlMsg, HostStats, InputMode, Profile } from "./protocol";
import { Session } from "./session";

const $ = <T extends HTMLElement>(id: string): T => {
  const el = document.getElementById(id);
  if (!el) throw new Error(`missing #${id}`);
  return el as T;
};

const connectScreen = $("connect-screen");
const viewerScreen = $("viewer-screen");
const hostInput = $<HTMLInputElement>("host");
const pinInput = $<HTMLInputElement>("pin");
const nameInput = $<HTMLInputElement>("client-name");
const connectBtn = $<HTMLButtonElement>("connect-btn");
const statusEl = $("status");
const pairedHint = $("paired-hint");
const canvas = $<HTMLCanvasElement>("screen");
const statsOverlay = $("stats-overlay");
const inputDenied = $("input-denied");
const toast = $("toast");

let session: Session | null = null;
let renderer: Renderer | null = null;
let input: InputCapture | null = null;
let statsTimer: number | undefined;
let pingTimer: number | undefined;
const clock = new ClockSync();
let hostStats: HostStats | null = null;
let inputAllowed = false;

function defaultHost(): string {
  // When served by nebulad itself, the page origin *is* the host.
  if (location.hostname && location.protocol.startsWith("http")) return location.host;
  return "";
}

function prefill(): void {
  const params = new URLSearchParams(location.search);
  hostInput.value = params.get("host") ?? defaultHost();
  pinInput.value = params.get("pin") ?? "";
  nameInput.value = storage.get("ndsp.clientName") ?? defaultDeviceName();
  updatePairedHint();
}

function defaultDeviceName(): string {
  const ua = navigator.userAgent;
  if (/android/i.test(ua)) return "Android device";
  if (/iphone|ipad/i.test(ua)) return "iOS device";
  if (/mac/i.test(ua)) return "Mac browser";
  if (/windows/i.test(ua)) return "Windows browser";
  return "Browser viewer";
}

function updatePairedHint(): void {
  const host = hostInput.value.trim();
  if (host && loadCredentials(host)) {
    pairedHint.textContent = "✓ This device is already paired with that host — PIN not needed.";
    $("pin-field").style.display = "none";
  } else {
    pairedHint.textContent = "";
    $("pin-field").style.display = "";
  }
}

function setStatus(text: string, kind: "" | "err" | "ok" = ""): void {
  statusEl.textContent = text;
  statusEl.className = `status ${kind}`;
}

function showToast(text: string, ms = 4000): void {
  toast.textContent = text;
  toast.style.display = "block";
  window.setTimeout(() => (toast.style.display = "none"), ms);
}

async function connect(): Promise<void> {
  const host = hostInput.value.trim();
  if (!host) return setStatus("Enter the host address shown on the PC.", "err");
  const paired = loadCredentials(host) !== null;
  const pin = pinInput.value.trim();
  if (!paired && !/^\d{4,10}$/.test(pin)) {
    return setStatus("Enter the PIN shown in the host's control panel.", "err");
  }
  storage.set("ndsp.clientName", nameInput.value.trim() || defaultDeviceName());

  connectBtn.disabled = true;
  setStatus(paired ? "Reconnecting with stored trust…" : "Pairing…");
  try {
    renderer = new Renderer(canvas);
    const s = await Session.connect(host, paired ? null : pin, nameInput.value.trim(), {
      onVideo: (frame) => void renderer?.push(frame),
      onControl: onControl,
      onClose: (reason) => endSession(reason),
    });
    session = s;
    renderer.requestKeyframe = () => void s.send({ type: "request_keyframe" });
    renderer.onError = (e) => {
      console.error("render error", e);
      showToast(`Video error: ${e.message}`, 8000);
    };
    inputAllowed = s.info.inputAllowed;
    enterViewer(s);
    setStatus("");
  } catch (e) {
    renderer?.destroy();
    renderer = null;
    setStatus((e as Error).message, "err");
    updatePairedHint(); // stale creds may have been cleared
  } finally {
    connectBtn.disabled = false;
  }
}

function onControl(msg: ControlMsg): void {
  switch (msg.type) {
    case "pong":
      clock.onPong(Number(msg.t0_us), Number(msg.t1_us));
      break;
    case "host_stats":
      hostStats = msg.stats as HostStats;
      break;
    case "input_grant": {
      inputAllowed = Boolean(msg.allowed);
      refreshInputBadge();
      showToast(inputAllowed ? "Host granted input control" : "Host revoked input control");
      break;
    }
    case "mode_change":
      break; // canvas auto-fits per frame
    case "bye":
      endSession(`Host ended the session: ${String(msg.reason)}`);
      break;
    case "error":
      console.warn("host error", msg);
      break;
  }
}

function refreshInputBadge(): void {
  const wantsInput = (input?.mode ?? "view_only") !== "view_only";
  inputDenied.style.display = wantsInput && !inputAllowed ? "inline" : "none";
}

function enterViewer(s: Session): void {
  connectScreen.style.display = "none";
  viewerScreen.classList.add("active");
  $("server-name").textContent = `${s.info.serverName} · ${s.info.codec.toUpperCase()}`;
  if (s.info.newlyPaired) showToast("Paired ✓ — this device is now trusted by the host");

  input = new InputCapture(canvas, (events) => void s.send({ type: "input", events }));
  input.attach();
  refreshInputBadge();

  pingTimer = window.setInterval(() => {
    void s.send({ type: "ping", t0_us: Math.round(clock.nowUs()) });
  }, 1000);

  statsTimer = window.setInterval(() => {
    if (!renderer) return;
    const st = renderer.stats;
    const e2e = st.lastPresentedTsUs > 0n ? clock.latencyMs(st.lastPresentedTsUs) : null;
    void s.send({
      type: "stats",
      stats: {
        fps_decoded: round1(st.fpsDecoded),
        decode_ms_avg: round1(st.decodeMsAvg),
        queue_depth: st.queueDepth,
        frames_dropped: st.framesDropped,
        rtt_ms: round1(clock.rttMs),
        e2e_latency_ms: e2e === null ? 0 : round1(e2e),
      },
    });
    if (statsOverlay.classList.contains("visible")) {
      statsOverlay.textContent = [
        `codec      ${s.info.codec}`,
        `decode fps ${st.fpsDecoded.toFixed(1)}`,
        `decode avg ${st.decodeMsAvg.toFixed(1)} ms`,
        `rtt        ${clock.rttMs.toFixed(1)} ms`,
        `e2e        ${e2e === null ? "syncing…" : e2e.toFixed(1) + " ms"}`,
        `dropped    ${st.framesDropped}`,
        hostStats
          ? `host       ${hostStats.capture_fps.toFixed(0)} fps cap · enc ${hostStats.encode_ms_avg.toFixed(1)} ms · ${(hostStats.actual_bitrate_kbps / 1000).toFixed(1)} Mbps`
          : "host       …",
      ].join("\n");
    }
  }, 1000);
}

function endSession(reason: string): void {
  if (!session) return;
  window.clearInterval(statsTimer);
  window.clearInterval(pingTimer);
  input?.detach();
  input = null;
  renderer?.destroy();
  renderer = null;
  session = null;
  hostStats = null;
  viewerScreen.classList.remove("active");
  connectScreen.style.display = "";
  setStatus(reason, reason.includes("error") || reason.includes("impostor") ? "err" : "");
  updatePairedHint();
}

function round1(n: number): number {
  return Math.round(n * 10) / 10;
}

// --- toolbar wiring -------------------------------------------------------
$("stats-btn").onclick = () => statsOverlay.classList.toggle("visible");
if (fullscreen.supported) {
  $("fullscreen-btn").onclick = () => {
    if (fullscreen.element()) void fullscreen.exit();
    else void fullscreen.enter(viewerScreen);
  };
} else {
  // iPhone Safari has no element fullscreen at all — hide the control.
  $("fullscreen-btn").style.display = "none";
}
$("disconnect-btn").onclick = () => {
  session?.close();
  endSession("Disconnected.");
};
$<HTMLSelectElement>("profile").onchange = (ev) => {
  const profile = (ev.target as HTMLSelectElement).value as Profile;
  void session?.send({ type: "set_profile", profile });
};
$<HTMLSelectElement>("input-mode").onchange = (ev) => {
  const mode = (ev.target as HTMLSelectElement).value as InputMode;
  if (input) input.mode = mode;
  void session?.send({ type: "set_input_mode", mode });
  refreshInputBadge();
};

connectBtn.onclick = () => void connect();
hostInput.oninput = updatePairedHint;
pinInput.onkeydown = (ev) => {
  if (ev.key === "Enter") void connect();
};

// Capability diagnostics: always in the console, and surfaced in the UI when
// running with fallbacks (the normal case for plain-HTTP LAN serving).
console.info(capabilityReport());
if (!usingNativeCrypto || !caps.persistentStorage) {
  const notes: string[] = [];
  if (!usingNativeCrypto) notes.push("built-in crypto (non-secure page context)");
  if (!caps.persistentStorage) notes.push("no persistent storage — pairing forgotten on reload");
  const el = document.getElementById("compat-note");
  if (el) {
    el.textContent = `Compatibility mode: ${notes.join(" · ")}`;
    el.style.display = "";
  }
}

prefill();
// QR deep link: auto-connect when both host and pin arrived via URL.
if (new URLSearchParams(location.search).get("pin") && hostInput.value) {
  void connect();
}
