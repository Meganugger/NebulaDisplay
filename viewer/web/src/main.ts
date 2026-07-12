// Viewer application: connect card → streaming canvas with toolbar/stats.

import "./style.css";
import { AudioPlayer } from "./audioplay";
import { capabilityReport, caps, fullscreen, storage } from "./caps";
import { ClockSync } from "./clock";
import { loadCredentials } from "./crypto";
import { usingNativeCrypto } from "./cryptobox";
import { Renderer } from "./decoder";
import { FileDropSender } from "./filedrop";
import { contentBox, InputCapture } from "./input";
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
const remoteCursor = $<HTMLImageElement>("remote-cursor");

let session: Session | null = null;
let renderer: Renderer | null = null;
let input: InputCapture | null = null;
let statsTimer: number | undefined;
let pingTimer: number | undefined;
const clock = new ClockSync();
let hostStats: HostStats | null = null;
let inputAllowed = false;
/** EMA of capture→arrival (host pipeline + network) per video envelope, ms. */
let netMsAvg = 0;
/** Host cursor overlay state (cursor channel). */
let cursorHot = { x: 0, y: 0 };
let cursorPos: { x: number; y: number; visible: boolean } | null = null;
/** Host audio playback (channel 3). */
let audio: AudioPlayer | null = null;
let audioWanted = false;
const audioBtn = $<HTMLButtonElement>("audio-btn");
const volumeSlider = $<HTMLInputElement>("volume");
/** File-drop sender for the current session. */
let fileDrop: FileDropSender | null = null;
const dropHint = $("drop-hint");
/** Clipboard sync state (suppress echo loops). */
let clipboardAllowed = false;
let lastClipboardSent = "";
let lastClipboardReceived = "";

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
    await openSession(host, paired ? null : pin);
    setStatus("");
  } catch (e) {
    setStatus((e as Error).message, "err");
    updatePairedHint(); // stale creds may have been cleared
  } finally {
    connectBtn.disabled = false;
  }
}

/** Establish a session and enter the viewer. Throws on failure. */
async function openSession(host: string, pin: string | null): Promise<void> {
  try {
    renderer = new Renderer(canvas);
    const s = await Session.connect(host, pin, nameInput.value.trim(), {
      onVideo: (frame) => {
        // Capture-timestamp → arrival: everything upstream of the decoder
        // (host capture/encode/seal/send + network), against the synced clock.
        const lat = clock.latencyMs(frame.timestampUs);
        if (lat !== null && lat >= 0) netMsAvg = netMsAvg === 0 ? lat : netMsAvg * 0.9 + lat * 0.1;
        void renderer?.push(frame);
      },
      onAudio: (frame) => audio?.push(frame),
      onControl: onControl,
      onClose: (reason) => onSessionClosed(host, reason),
    });
    session = s;
    fileDrop = new FileDropSender(s);
    clipboardAllowed = s.info.clipboardAllowed;
    renderer.requestKeyframe = () => void s.send({ type: "request_keyframe" });
    renderer.onError = (e) => {
      console.error("render error", e);
      showToast(`Video error: ${e.message}`, 8000);
    };
    inputAllowed = s.info.inputAllowed;
    userDisconnected = false; // fresh session — clear any stale flag
    enterViewer(s);
  } catch (e) {
    renderer?.destroy();
    renderer = null;
    throw e;
  }
}

let userDisconnected = false;
let reconnecting = false;

/**
 * Wi-Fi blips and host restarts shouldn't dump the user back at the connect
 * form: when a session closes unexpectedly and stored trust exists, quietly
 * retry (token reconnect, no PIN) with short backoff before giving up.
 */
function onSessionClosed(host: string, reason: string): void {
  if (!session) return; // already handled
  const wasUser = userDisconnected;
  userDisconnected = false;
  endSession(reason);
  if (wasUser || reconnecting || loadCredentials(host) === null) return;
  if (/impostor|protocol error|revoked/i.test(reason)) return; // not transient
  void (async () => {
    reconnecting = true;
    try {
      for (let attempt = 1; attempt <= 5; attempt++) {
        showToast(`Connection lost — reconnecting (${attempt}/5)…`, 2500);
        await new Promise((r) => setTimeout(r, attempt === 1 ? 300 : 1500 * (attempt - 1)));
        try {
          await openSession(host, null);
          showToast("Reconnected ✓");
          setStatus("");
          return;
        } catch {
          if (loadCredentials(host) === null) break; // trust cleared — needs PIN
        }
      }
      setStatus("Connection lost. Reconnect manually.", "err");
    } finally {
      reconnecting = false;
    }
  })();
}

function onControl(msg: ControlMsg): void {
  switch (msg.type) {
    case "clipboard_data":
      void applyHostClipboard(msg);
      break;
    case "clipboard_grant": {
      clipboardAllowed = Boolean(msg.allowed);
      showToast(
        clipboardAllowed ? "Host enabled clipboard sync" : "Host disabled clipboard sync",
      );
      break;
    }
    case "audio_start":
      showToast("Host audio on 🔊");
      break;
    case "audio_stop":
      if (audioWanted) showToast("Host audio stopped");
      break;
    case "file_accept":
      fileDrop?.onAccept(Number(msg.transfer_id));
      break;
    case "file_reject":
      fileDrop?.onReject(Number(msg.transfer_id), String(msg.reason ?? "rejected"));
      break;
    case "file_done":
      fileDrop?.onDone(
        Number(msg.transfer_id),
        Boolean(msg.ok),
        msg.error ? String(msg.error) : undefined,
      );
      break;
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
    case "cursor_shape": {
      // RGBA8 → canvas → data URL for the overlay <img>.
      const w = Number(msg.width);
      const h = Number(msg.height);
      cursorHot = { x: Number(msg.hot_x), y: Number(msg.hot_y) };
      try {
        const bytes = Uint8Array.from(atob(String(msg.rgba)), (c) => c.charCodeAt(0));
        const cnv = document.createElement("canvas");
        cnv.width = w;
        cnv.height = h;
        const cctx = cnv.getContext("2d");
        if (cctx) {
          cctx.putImageData(new ImageData(new Uint8ClampedArray(bytes.buffer), w, h), 0, 0);
          remoteCursor.src = cnv.toDataURL();
        }
      } catch (e) {
        console.warn("cursor shape decode failed", e);
      }
      break;
    }
    case "cursor_pos":
      cursorPos = { x: Number(msg.x), y: Number(msg.y), visible: Boolean(msg.visible) };
      placeRemoteCursor();
      break;
    case "mode_change":
      break; // canvas auto-fits per frame
    case "bye":
      endSession(`Host ended the session: ${String(msg.reason)}`);
      break;
    case "error":
      if (msg.code === "audio_disabled") {
        showToast("Audio is disabled in the host's config (audio = false)");
        setAudioEnabled(false);
      }
      console.warn("host error", msg);
      break;
  }
}

/** Host clipboard arrived — write it locally (needs focus in most browsers). */
async function applyHostClipboard(msg: ControlMsg): Promise<void> {
  if (String(msg.format) !== "text" || !navigator.clipboard) return;
  try {
    const bytes = Uint8Array.from(atob(String(msg.data)), (c) => c.charCodeAt(0));
    const text = new TextDecoder().decode(bytes);
    lastClipboardReceived = text;
    await navigator.clipboard.writeText(text);
    showToast("📋 Clipboard received from host");
  } catch (e) {
    // Usually a focus/permission constraint; the text will land on the next
    // successful write attempt after the user focuses the page.
    console.warn("clipboard write failed", e);
  }
}

/**
 * Push the local clipboard to the host when it changed. Called on focus and
 * shortly after copy/cut (the event fires before the clipboard is updated).
 */
async function maybeSendClipboard(): Promise<void> {
  if (!session || !clipboardAllowed || !navigator.clipboard?.readText) return;
  let text: string;
  try {
    text = await navigator.clipboard.readText();
  } catch {
    return; // permission denied / not focused
  }
  if (!text || text === lastClipboardSent || text === lastClipboardReceived) return;
  if (new TextEncoder().encode(text).length > 256 * 1024) return; // host cap
  lastClipboardSent = text;
  void session.send({ type: "clipboard_data", format: "text", data: b64text(text) });
}

function b64text(text: string): string {
  const bytes = new TextEncoder().encode(text);
  let s = "";
  for (const b of bytes) s += String.fromCharCode(b);
  return btoa(s);
}

function setAudioEnabled(on: boolean): void {
  audioWanted = on;
  audioBtn.textContent = on ? "🔊" : "🔇";
  volumeSlider.style.display = on ? "" : "none";
  if (on) {
    audio?.stop();
    audio = new AudioPlayer();
    audio.start(48000, 2, Number(volumeSlider.value) / 100);
  } else {
    audio?.stop();
    audio = null;
  }
  void session?.send({ type: "set_audio", enabled: on });
}

/**
 * Position the host-cursor overlay: normalized capture coordinates → screen
 * pixels of the letterboxed content box, hotspot-adjusted, scaled with the
 * video. A transform (not left/top) keeps this off the layout path.
 */
function placeRemoteCursor(): void {
  const p = cursorPos;
  if (!p || !p.visible || !remoteCursor.src) {
    remoteCursor.style.display = "none";
    return;
  }
  const r = canvas.getBoundingClientRect();
  const box = contentBox(
    { left: r.left, top: r.top, width: r.width, height: r.height },
    canvas.width,
    canvas.height,
  );
  const x = box.left + p.x * box.width - cursorHot.x * box.scale;
  const y = box.top + p.y * box.height - cursorHot.y * box.scale;
  remoteCursor.style.display = "block";
  remoteCursor.style.transform = `translate(${x}px, ${y}px) scale(${box.scale})`;
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

  input = new InputCapture(
    canvas,
    (events) => void s.send({ type: "input", events }),
    // Canvas backing-store size == native video size (the renderer keeps
    // them in sync), which is what letterbox mapping needs.
    () => ({ w: canvas.width, h: canvas.height }),
  );
  input.attach();
  refreshInputBadge();

  // Host audio: only offer the toggle when the host has audio enabled and
  // this browser can decode Opus (WebCodecs — secure contexts).
  const audioSupported = AudioPlayer.supported() && s.info.audioAvailable;
  audioBtn.style.display = audioSupported ? "" : "none";
  audioBtn.textContent = "🔇";
  volumeSlider.style.display = "none";

  // Clipboard sync: push local changes on focus / copy / cut.
  const clipHandler = () => setTimeout(() => void maybeSendClipboard(), 120);
  window.addEventListener("focus", clipHandler);
  document.addEventListener("copy", clipHandler);
  document.addEventListener("cut", clipHandler);
  sessionDisposers.push(() => {
    window.removeEventListener("focus", clipHandler);
    document.removeEventListener("copy", clipHandler);
    document.removeEventListener("cut", clipHandler);
  });

  // File drop: drag a file anywhere over the viewer.
  const onDragOver = (ev: DragEvent) => {
    if (!ev.dataTransfer?.types.includes("Files")) return;
    ev.preventDefault();
    dropHint.classList.add("visible");
  };
  const onDragLeave = () => dropHint.classList.remove("visible");
  const onDrop = (ev: DragEvent) => {
    ev.preventDefault();
    dropHint.classList.remove("visible");
    const files = ev.dataTransfer?.files;
    if (!files?.length || !fileDrop) return;
    for (const file of files) {
      void fileDrop.offer(file, (p) => {
        switch (p.state) {
          case "hashing":
            showToast(`Preparing ${p.name}…`);
            break;
          case "waiting":
            showToast(`Waiting for the host to accept ${p.name} — check the host panel`, 8000);
            break;
          case "sending": {
            const pct = p.size ? Math.round((p.sent / p.size) * 100) : 0;
            showToast(`Sending ${p.name}… ${pct}%`, 1500);
            break;
          }
          case "done":
            showToast(`✓ ${p.name} delivered`);
            break;
          case "failed":
            showToast(`✗ ${p.name}: ${p.error ?? "failed"}`, 8000);
            break;
        }
      });
    }
  };
  viewerScreen.addEventListener("dragover", onDragOver);
  viewerScreen.addEventListener("dragleave", onDragLeave);
  viewerScreen.addEventListener("drop", onDrop);
  sessionDisposers.push(() => {
    viewerScreen.removeEventListener("dragover", onDragOver);
    viewerScreen.removeEventListener("dragleave", onDragLeave);
    viewerScreen.removeEventListener("drop", onDrop);
  });

  // 2 Hz keeps the clock/RTT estimate fresh enough for adaptation without
  // measurable cost.
  pingTimer = window.setInterval(() => {
    void s.send({ type: "ping", t0_us: Math.round(clock.nowUs()) });
  }, 500);

  statsTimer = window.setInterval(() => {
    if (!renderer) return;
    const st = renderer.stats;
    // Latency of the last presented frame *at the moment it was painted* —
    // latencyMs() alone would add however long ago that paint happened
    // (up to a full frame interval of pure measurement error).
    const sincePaintMs = performance.now() - st.lastPresentedAtMs;
    const raw = st.lastPresentedTsUs > 0n ? clock.latencyMs(st.lastPresentedTsUs) : null;
    const e2e = raw === null ? null : Math.max(0, raw - sincePaintMs);
    void s.send({
      type: "stats",
      stats: {
        fps_decoded: round1(st.fpsDecoded),
        decode_ms_avg: round1(st.decodeMsAvg),
        queue_depth: st.queueDepth,
        frames_dropped: st.framesDropped,
        rtt_ms: round1(clock.rttMs),
        e2e_latency_ms: e2e === null ? 0 : round1(e2e),
        net_ms_avg: round1(netMsAvg),
        present_wait_ms_avg: round1(st.presentWaitMsAvg),
      },
    });
    if (statsOverlay.classList.contains("visible")) {
      statsOverlay.textContent = [
        `codec      ${s.info.codec}`,
        `decode fps ${st.fpsDecoded.toFixed(1)}`,
        `decode avg ${st.decodeMsAvg.toFixed(1)} ms`,
        `rtt        ${clock.rttMs.toFixed(1)} ms`,
        `e2e        ${e2e === null ? "syncing…" : e2e.toFixed(1) + " ms"}`,
        `net+host   ${netMsAvg.toFixed(1)} ms`,
        `present    ${st.presentWaitMsAvg.toFixed(2)} ms`,
        `dropped    ${st.framesDropped}`,
        hostStats
          ? `host       ${hostStats.capture_fps.toFixed(0)} fps cap · enc ${hostStats.encode_ms_avg.toFixed(1)} ms (cvt ${hostStats.convert_ms_avg.toFixed(1)}) · age ${hostStats.capture_age_ms_avg.toFixed(1)} · send ${hostStats.seal_send_ms_avg.toFixed(1)} · ${(hostStats.actual_bitrate_kbps / 1000).toFixed(1)} Mbps`
          : "host       …",
      ].join("\n");
    }
  }, 1000);
}

let sessionDisposers: (() => void)[] = [];

function endSession(reason: string): void {
  if (!session) return;
  window.clearInterval(statsTimer);
  window.clearInterval(pingTimer);
  input?.detach();
  input = null;
  renderer?.destroy();
  renderer = null;
  session = null;
  for (const d of sessionDisposers) d();
  sessionDisposers = [];
  audio?.stop();
  audio = null;
  audioWanted = false;
  audioBtn.style.display = "none";
  volumeSlider.style.display = "none";
  fileDrop = null;
  clipboardAllowed = false;
  lastClipboardSent = "";
  lastClipboardReceived = "";
  dropHint.classList.remove("visible");
  hostStats = null;
  cursorPos = null;
  remoteCursor.style.display = "none";
  remoteCursor.removeAttribute("src");
  viewerScreen.classList.remove("active");
  connectScreen.style.display = "";
  setStatus(reason, reason.includes("error") || reason.includes("impostor") ? "err" : "");
  updatePairedHint();
}

function round1(n: number): number {
  return Math.round(n * 10) / 10;
}

// --- toolbar wiring -------------------------------------------------------
audioBtn.onclick = () => setAudioEnabled(!audioWanted);
volumeSlider.oninput = () => audio?.setVolume(Number(volumeSlider.value) / 100);
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
  userDisconnected = true;
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
