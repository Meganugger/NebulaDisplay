// Viewer application: connect card → streaming canvas with toolbar/stats.

import "./style.css";
import { AudioPlayer, audioPreference } from "./audio";
import { capabilityReport, caps, fullscreen, storage } from "./caps";
import { ClockSync } from "./clock";
import { loadCredentials } from "./crypto";
import { usingNativeCrypto } from "./cryptobox";
import { Renderer } from "./decoder";
import { FileReceiver, FileSender } from "./filedrop";
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
let audioPlayer: AudioPlayer | null = null;
let audioWanted = false;
let fileSender: FileSender | null = null;
let fileReceiver: FileReceiver | null = null;
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
      onAudio: (frame) => audioPlayer?.push(frame),
      onControl: onControl,
      onClose: (reason) => onSessionClosed(host, reason),
    });
    session = s;
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

function fmtBytes(n: number): string {
  if (n >= 1024 * 1024 * 1024) return `${(n / (1024 * 1024 * 1024)).toFixed(1)} GiB`;
  if (n >= 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MiB`;
  if (n >= 1024) return `${(n / 1024).toFixed(0)} KiB`;
  return `${n} B`;
}

function onControl(msg: ControlMsg): void {
  // Transfer messages are consumed by the active file sender, or — for
  // host-initiated sends (file_offer and its follow-ups) — the receiver.
  if (fileSender?.handleControl(msg)) return;
  if (fileReceiver?.handleControl(msg)) return;
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
    case "audio_grant": {
      if (!msg.allowed && audioWanted) {
        showToast("Host muted audio for this device");
        stopAudio(false);
      }
      break;
    }
    case "clipboard": {
      onRemoteClipboard(String(msg.text));
      break;
    }
    case "clipboard_grant": {
      showToast(
        msg.allowed
          ? "Host enabled clipboard sync for this device"
          : "Host disabled clipboard sync for this device",
      );
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
      console.warn("host error", msg);
      break;
  }
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
  fileSender = new FileSender(s);
  fileReceiver = new FileReceiver(s, {
    confirmOffer: (name, size) =>
      Promise.resolve(
        window.confirm(`The host wants to send you a file:\n\n${name} (${fmtBytes(size)})\n\nAccept?`),
      ),
    onStatus: (text) => showToast(text),
    onProgress: () => {},
    onFile: (name, blob) => {
      const url = URL.createObjectURL(blob);
      const a = document.createElement("a");
      a.href = url;
      a.download = name;
      document.body.appendChild(a);
      a.click();
      a.remove();
      setTimeout(() => URL.revokeObjectURL(url), 60_000);
      showToast(`Received ${name} ✓`);
    },
    onFail: (reason) => showToast(`File receive failed: ${reason}`, 6000),
  });
  const audioBtn = $<HTMLButtonElement>("audio-btn");
  if (audioPreference() === null) {
    audioBtn.style.display = "none"; // nothing here can play audio
  } else {
    audioBtn.style.display = "";
    audioBtn.textContent = "🔇";
    audioBtn.classList.remove("on");
  }

  input = new InputCapture(
    canvas,
    (events) => void s.send({ type: "input", events }),
    // Canvas backing-store size == native video size (the renderer keeps
    // them in sync), which is what letterbox mapping needs.
    () => ({ w: canvas.width, h: canvas.height }),
  );
  input.attach();
  refreshInputBadge();

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

function endSession(reason: string): void {
  if (!session) return;
  window.clearInterval(statsTimer);
  window.clearInterval(pingTimer);
  input?.detach();
  input = null;
  stopAudio(false);
  fileSender = null;
  fileReceiver = null;
  transferStatus.style.display = "none";
  renderer?.destroy();
  renderer = null;
  session = null;
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

// --- audio ------------------------------------------------------------------

const volumeSlider = $<HTMLInputElement>("volume");

async function startAudio(): Promise<void> {
  const pref = audioPreference();
  if (!session || !pref) return;
  try {
    audioPlayer = new AudioPlayer(pref.codec);
    audioPlayer.onError = () => {
      showToast("Audio decoder failed — audio stopped", 6000);
      stopAudio(true);
    };
    audioPlayer.setVolume(Number(volumeSlider.value) / 100);
    await audioPlayer.resume(); // inside the click gesture (autoplay policy)
    audioWanted = true;
    void session.send({ type: "set_audio", enabled: true, codec: pref.codec });
    $("audio-btn").textContent = "🔊";
    $("audio-btn").classList.add("on");
    volumeSlider.style.display = "";
  } catch (e) {
    showToast(`Audio unavailable: ${(e as Error).message}`, 6000);
    audioPlayer?.destroy();
    audioPlayer = null;
  }
}

function stopAudio(tellHost: boolean): void {
  if (tellHost || audioWanted) {
    void session?.send({ type: "set_audio", enabled: false });
  }
  audioWanted = false;
  audioPlayer?.destroy();
  audioPlayer = null;
  const btn = document.getElementById("audio-btn");
  if (btn) {
    btn.textContent = "🔇";
    btn.classList.remove("on");
  }
  volumeSlider.style.display = "none";
}

$("audio-btn").onclick = () => {
  if (audioWanted) stopAudio(true);
  else void startAudio();
};
volumeSlider.oninput = () => audioPlayer?.setVolume(Number(volumeSlider.value) / 100);

// --- clipboard ---------------------------------------------------------------

/** Best-effort local clipboard read: async API on secure origins, manual
 *  paste prompt everywhere else (insecure LAN origins have no readText). */
async function readLocalClipboard(): Promise<string | null> {
  if (navigator.clipboard?.readText) {
    try {
      return await navigator.clipboard.readText();
    } catch {
      /* permission denied — fall through to the prompt */
    }
  }
  return window.prompt("Paste the text to send to the PC's clipboard:");
}

$("clip-btn").onclick = () => {
  void (async () => {
    if (!session) return;
    const text = await readLocalClipboard();
    if (!text) return;
    if (text.length > 256 * 1024) {
      showToast("Clipboard too large (256 KiB max)", 5000);
      return;
    }
    void session.send({ type: "clipboard", text });
    showToast("Clipboard sent to the PC (host permission required)");
  })();
};

function onRemoteClipboard(text: string): void {
  // Try the silent path first; browsers may require a gesture — then we
  // show a toast with an explicit copy button (execCommand works there).
  const fallback = () => {
    toast.textContent = "PC clipboard received ";
    const btn = document.createElement("button");
    btn.textContent = "Copy here";
    btn.onclick = () => {
      const ta = document.createElement("textarea");
      ta.value = text;
      ta.style.position = "fixed";
      ta.style.opacity = "0";
      document.body.appendChild(ta);
      ta.select();
      try {
        document.execCommand("copy");
        showToast("Copied ✓");
      } finally {
        ta.remove();
      }
    };
    toast.appendChild(btn);
    toast.style.display = "block";
    window.setTimeout(() => (toast.style.display = "none"), 8000);
  };
  if (navigator.clipboard?.writeText) {
    navigator.clipboard.writeText(text).then(
      () => showToast("PC clipboard synced to this device 📋"),
      fallback,
    );
  } else {
    fallback();
  }
}

// --- file drop -----------------------------------------------------------------

const dropOverlay = $("drop-overlay");
const transferStatus = $("transfer-status");

function xferUi() {
  return {
    onStatus(text: string) {
      transferStatus.textContent = text;
      transferStatus.style.display = "block";
    },
    onProgress(sent: number, total: number) {
      const pct = total ? Math.floor((sent / total) * 100) : 0;
      transferStatus.textContent = `Sending… ${pct}% (${(sent / 1e6).toFixed(1)} / ${(total / 1e6).toFixed(1)} MB)`;
      transferStatus.style.display = "block";
    },
    onDone(name: string) {
      transferStatus.style.display = "none";
      showToast(`${name} delivered to the PC ✓`, 6000);
    },
    onFail(reason: string) {
      transferStatus.style.display = "none";
      showToast(`File not sent: ${reason}`, 7000);
    },
  };
}

let dragDepth = 0;
viewerScreen.addEventListener("dragenter", (ev) => {
  ev.preventDefault();
  dragDepth++;
  dropOverlay.classList.add("visible");
});
viewerScreen.addEventListener("dragover", (ev) => ev.preventDefault());
viewerScreen.addEventListener("dragleave", () => {
  if (--dragDepth <= 0) {
    dragDepth = 0;
    dropOverlay.classList.remove("visible");
  }
});
viewerScreen.addEventListener("drop", (ev) => {
  ev.preventDefault();
  dragDepth = 0;
  dropOverlay.classList.remove("visible");
  const file = ev.dataTransfer?.files?.[0];
  if (!file || !fileSender) return;
  if (fileSender.busy) {
    showToast("A transfer is already running — wait for it to finish", 5000);
    return;
  }
  void fileSender.sendFile(file, xferUi());
});

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

// Test hook (E2E asserts audio actually decodes/schedules — not just that
// packets arrive). Carries no capability beyond what the UI already does.
(globalThis as Record<string, unknown>)["__ndspDebug"] = {
  audioFramesPlayed: () => audioPlayer?.framesPlayed ?? 0,
};

prefill();
// QR deep link: auto-connect when both host and pin arrived via URL.
if (new URLSearchParams(location.search).get("pin") && hostInput.value) {
  void connect();
}
