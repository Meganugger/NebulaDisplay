// Host control panel (served on 127.0.0.1 only by nebulad).

import "./style.css";
import { HostStats, ViewerStats } from "./protocol";

interface ClientView {
  id: number;
  device_id: string;
  name: string;
  platform: string;
  addr: string;
  connected_unix: number;
  input_allowed: boolean;
  clipboard_allowed: boolean;
  audio_on: boolean;
  stats: ViewerStats;
}

interface PendingFileView {
  client_id: number;
  transfer_id: number;
  device_id: string;
  device_name: string;
  file_name: string;
  size: number;
  offered_unix: number;
}

interface TrustedView {
  device_id: string;
  name: string;
  platform: string;
  created_unix: number;
  last_seen_unix: number;
  input_allowed: boolean;
  clipboard_allowed: boolean;
  online: boolean;
}

interface Status {
  name: string;
  version: string;
  fingerprint: string;
  port: number;
  pin: string;
  viewer_urls: string[];
  mode: { width: number; height: number; refresh_hz: number };
  host_stats: HostStats;
  clients: ClientView[];
  trusted: TrustedView[];
  pending_files: PendingFileView[];
  audio_available: boolean;
}

const $ = (id: string): HTMLElement => document.getElementById(id)!;

async function api<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(path, init);
  if (!res.ok) throw new Error(`${path}: ${res.status} ${await res.text()}`);
  const ct = res.headers.get("content-type") ?? "";
  return (ct.includes("json") ? res.json() : res.text()) as Promise<T>;
}

function esc(s: string): string {
  const d = document.createElement("span");
  d.textContent = s;
  return d.innerHTML;
}

function ago(unix: number): string {
  if (!unix) return "—";
  const s = Math.max(0, Date.now() / 1000 - unix);
  if (s < 90) return `${Math.round(s)}s ago`;
  if (s < 5400) return `${Math.round(s / 60)}m ago`;
  if (s < 129600) return `${Math.round(s / 3600)}h ago`;
  return `${Math.round(s / 86400)}d ago`;
}

let lastPin = "";

async function refresh(): Promise<void> {
  const st = await api<Status>("/api/status");

  if (st.pin !== lastPin) {
    lastPin = st.pin;
    $("pin").textContent = st.pin;
    ($("qr") as HTMLImageElement).src = `/api/qr.svg?ts=${Date.now()}`;
  }

  $("host-info").innerHTML = [
    line("Name", esc(st.name)),
    line("Version", esc(st.version)),
    line("Mode", `${st.mode.width}×${st.mode.height}`),
    line("Identity", `<span class="mono">${esc(st.fingerprint.slice(0, 16))}…</span>`),
  ].join("");
  $("urls").innerHTML =
    st.viewer_urls.map((u) => `<a href="${esc(u)}" target="_blank">${esc(u)}</a>`).join("<br>") ||
    `<span style="color:var(--fg-dim)">no LAN address detected — use this machine's IP with port ${st.port}</span>`;

  const hs = st.host_stats;
  $("host-stats").innerHTML = [
    line("Capture", `${hs.capture_fps.toFixed(0)} fps`),
    line("Encode", `${hs.encode_ms_avg.toFixed(1)} ms`),
    line("Bitrate", `${(hs.actual_bitrate_kbps / 1000).toFixed(1)} / ${(hs.target_bitrate_kbps / 1000).toFixed(1)} Mbps`),
    line("Frames sent", String(hs.frames_sent)),
    line("Backlogged", String(hs.frames_skipped)),
    line("Viewers", String(hs.clients)),
  ].join("");

  // Connected clients.
  const ctbody = $("clients").querySelector("tbody")!;
  ctbody.innerHTML = st.clients
    .map(
      (c) => `<tr>
        <td><b>${esc(c.name)}</b><br><span style="color:var(--fg-dim);font-size:0.75rem">${esc(c.platform)}</span></td>
        <td class="mono">${esc(c.addr)}</td>
        <td class="mono">${c.stats.fps_decoded.toFixed(0)}</td>
        <td class="mono">${c.stats.e2e_latency_ms ? c.stats.e2e_latency_ms.toFixed(0) + " ms" : "—"}</td>
        <td class="mono">${c.stats.rtt_ms ? c.stats.rtt_ms.toFixed(0) + " ms" : "—"}</td>
        <td><span class="tag ${c.input_allowed ? "on" : "off"}">${c.input_allowed ? "granted" : "view-only"}</span></td>
        <td><span class="tag ${c.audio_on ? "on" : "off"}">${c.audio_on ? "🔊 live" : "off"}</span></td>
      </tr>`,
    )
    .join("");
  $("no-clients").style.display = st.clients.length ? "none" : "";

  // Trusted devices.
  const ttbody = $("trusted").querySelector("tbody")!;
  ttbody.innerHTML = st.trusted
    .map(
      (d) => `<tr>
        <td><b>${esc(d.name)}</b> ${d.online ? '<span class="tag on">online</span>' : ""}</td>
        <td>${esc(d.platform)}</td>
        <td>${ago(d.last_seen_unix)}</td>
        <td>
          <label class="switch"><input type="checkbox" data-grant="${esc(d.device_id)}" ${d.input_allowed ? "checked" : ""}><span></span></label>
        </td>
        <td>
          <label class="switch"><input type="checkbox" data-clipgrant="${esc(d.device_id)}" ${d.clipboard_allowed ? "checked" : ""}><span></span></label>
        </td>
        <td><button class="danger" data-revoke="${esc(d.device_id)}">Revoke</button></td>
      </tr>`,
    )
    .join("");
  $("no-trusted").style.display = st.trusted.length ? "none" : "";

  ttbody.querySelectorAll<HTMLInputElement>("input[data-grant]").forEach((el) => {
    el.onchange = () =>
      void api("/api/grant", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ device_id: el.dataset["grant"], allowed: el.checked, what: "input" }),
      }).catch(console.error);
  });
  ttbody.querySelectorAll<HTMLInputElement>("input[data-clipgrant]").forEach((el) => {
    el.onchange = () =>
      void api("/api/grant", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          device_id: el.dataset["clipgrant"],
          allowed: el.checked,
          what: "clipboard",
        }),
      }).catch(console.error);
  });

  // Pending file-drop offers (explicit accept per transfer).
  const tile = $("pending-files-tile");
  tile.style.display = st.pending_files.length ? "" : "none";
  const ftbody = $("pending-files").querySelector("tbody")!;
  ftbody.innerHTML = st.pending_files
    .map(
      (f) => `<tr>
        <td><b>${esc(f.file_name)}</b></td>
        <td class="mono">${fmtSize(f.size)}</td>
        <td>${esc(f.device_name)}</td>
        <td>
          <button data-fdec="${f.client_id}:${f.transfer_id}:1">Accept</button>
          <button class="danger" data-fdec="${f.client_id}:${f.transfer_id}:0">Decline</button>
        </td>
      </tr>`,
    )
    .join("");
  ftbody.querySelectorAll<HTMLButtonElement>("button[data-fdec]").forEach((el) => {
    el.onclick = () => {
      const [clientId, transferId, accept] = el.dataset["fdec"]!.split(":");
      void api("/api/files/decide", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          client_id: Number(clientId),
          transfer_id: Number(transferId),
          accept: accept === "1",
        }),
      })
        .then(refresh)
        .catch(console.error);
    };
  });
  ttbody.querySelectorAll<HTMLButtonElement>("button[data-revoke]").forEach((el) => {
    el.onclick = () => {
      if (!confirm("Revoke this device? It will be disconnected and must pair again.")) return;
      void api("/api/revoke", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ device_id: el.dataset["revoke"] }),
      })
        .then(refresh)
        .catch(console.error);
    };
  });
}

function fmtSize(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KiB`;
  if (n < 1024 * 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MiB`;
  return `${(n / 1024 / 1024 / 1024).toFixed(2)} GiB`;
}

function line(k: string, v: string): string {
  return `<div class="statline"><span>${k}</span><span class="v">${v}</span></div>`;
}

$("rotate").onclick = () =>
  void api<{ pin: string }>("/api/pin/rotate", { method: "POST" }).then(refresh).catch(console.error);

void refresh().catch((e) => {
  document.body.innerHTML = `<div class="panel-wrap"><div class="tile"><h2>Panel unreachable</h2><p>${esc(String(e))}</p><p>The panel only works on the host machine itself (http://127.0.0.1).</p></div></div>`;
});
setInterval(() => void refresh().catch(console.error), 2000);
