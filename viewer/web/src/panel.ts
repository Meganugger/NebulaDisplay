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
  audio_allowed: boolean;
  audio_active: boolean;
  stats: ViewerStats;
  sending_file: boolean;
}

interface TrustedView {
  device_id: string;
  name: string;
  platform: string;
  created_unix: number;
  last_seen_unix: number;
  input_allowed: boolean;
  clipboard_allowed: boolean;
  audio_allowed: boolean;
  online: boolean;
}

let sendFileBusy = false;

/** Per-client "send file" flow: pick a file, stream it to the host, which
 *  offers it to the viewer (the viewer must accept before chunks flow). */
function sendFileTo(deviceId: string, deviceName: string): void {
  if (sendFileBusy) return;
  const input = $("send-file-input") as HTMLInputElement;
  input.value = "";
  input.onchange = () => {
    const file = input.files?.[0];
    if (!file) return;
    sendFileBusy = true;
    void (async () => {
      try {
        const q = `device_id=${encodeURIComponent(deviceId)}&name=${encodeURIComponent(file.name)}`;
        await api(`/api/send_file?${q}`, { method: "POST", body: file });
        alert(`Offered "${file.name}" to ${deviceName} — they must accept it on their side.`);
      } catch (e) {
        alert(`Send failed: ${(e as Error).message}`);
      } finally {
        sendFileBusy = false;
      }
    })();
  };
  input.click();
}

interface PendingTransfer {
  id: string;
  device_id: string;
  device_name: string;
  name: string;
  size_bytes: number;
  offered_unix: number;
  expires_in_secs: number;
}

interface Status {
  name: string;
  version: string;
  fingerprint: string;
  tls_fingerprint: string | null;
  audio_enabled: boolean;
  port: number;
  pin: string;
  viewer_urls: string[];
  pending_transfers: PendingTransfer[];
  mode: { width: number; height: number; refresh_hz: number };
  host_stats: HostStats;
  clients: ClientView[];
  trusted: TrustedView[];
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
    line("Audio", st.audio_enabled ? "available (viewers opt in)" : "disabled"),
    ...(st.tls_fingerprint
      ? [line("TLS cert", `<span class="mono" title="${esc(st.tls_fingerprint)}">${esc(st.tls_fingerprint.slice(0, 23))}…</span>`)]
      : []),
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
        <td><b>${esc(c.name)}</b>${c.audio_active ? ' <span class="tag on" title="This device is receiving the PC\u2019s audio right now">🔊 listening</span>' : ""}<br><span style="color:var(--fg-dim);font-size:0.75rem">${esc(c.platform)}</span></td>
        <td class="mono">${esc(c.addr)}</td>
        <td class="mono">${c.stats.fps_decoded.toFixed(0)}</td>
        <td class="mono">${c.stats.e2e_latency_ms ? c.stats.e2e_latency_ms.toFixed(0) + " ms" : "—"}</td>
        <td class="mono">${c.stats.rtt_ms ? c.stats.rtt_ms.toFixed(0) + " ms" : "—"}</td>
        <td><span class="tag ${c.input_allowed ? "on" : "off"}">${c.input_allowed ? "granted" : "view-only"}</span></td>
        <td><button data-sendfile="${esc(c.device_id)}" data-name="${esc(c.name)}" ${c.sending_file ? "disabled" : ""}>${c.sending_file ? "sending…" : "Send file"}</button></td>
      </tr>`,
    )
    .join("");
  ctbody.querySelectorAll<HTMLButtonElement>("button[data-sendfile]").forEach((el) => {
    el.onclick = () => sendFileTo(el.dataset.sendfile!, el.dataset.name ?? "viewer");
  });
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
          <label class="switch" title="Allow input injection"><input type="checkbox" data-grant="${esc(d.device_id)}" data-kind="input" ${d.input_allowed ? "checked" : ""}><span></span></label>
        </td>
        <td>
          <label class="switch" title="Allow clipboard sync"><input type="checkbox" data-grant="${esc(d.device_id)}" data-kind="clipboard" ${d.clipboard_allowed ? "checked" : ""}><span></span></label>
        </td>
        <td>
          <label class="switch" title="Allow audio streaming"><input type="checkbox" data-grant="${esc(d.device_id)}" data-kind="audio" ${d.audio_allowed ? "checked" : ""}><span></span></label>
        </td>
        <td><button class="danger" data-revoke="${esc(d.device_id)}">Revoke</button></td>
      </tr>`,
    )
    .join("");
  $("no-trusted").style.display = st.trusted.length ? "none" : "";

  renderTransfers(st.pending_transfers);

  ttbody.querySelectorAll<HTMLInputElement>("input[data-grant]").forEach((el) => {
    el.onchange = () =>
      void api("/api/grant", {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          device_id: el.dataset["grant"],
          allowed: el.checked,
          kind: el.dataset["kind"] ?? "input",
        }),
      }).catch(console.error);
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

function fmtBytes(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KiB`;
  if (n < 1024 ** 3) return `${(n / 1024 ** 2).toFixed(1)} MiB`;
  return `${(n / 1024 ** 3).toFixed(2)} GiB`;
}

function renderTransfers(pending: PendingTransfer[]): void {
  const box = $("pending-transfers");
  const tile = $("transfers-tile");
  tile.style.display = pending.length ? "" : "none";
  box.innerHTML = pending
    .map(
      (t) => `<div class="xfer">
        <div class="meta">
          <b>${esc(t.name)}</b>
          <small>${fmtBytes(t.size_bytes)} · from ${esc(t.device_name)} · expires in ${t.expires_in_secs}s</small>
        </div>
        <button data-xfer-accept="${esc(t.id)}">Accept</button>
        <button class="danger" data-xfer-deny="${esc(t.id)}">Deny</button>
      </div>`,
    )
    .join("");
  const answer = (id: string, accept: boolean) =>
    void api("/api/transfers/answer", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ id, accept }),
    })
      .then(refresh)
      .catch(console.error);
  box.querySelectorAll<HTMLButtonElement>("button[data-xfer-accept]").forEach((el) => {
    el.onclick = () => answer(el.dataset["xferAccept"]!, true);
  });
  box.querySelectorAll<HTMLButtonElement>("button[data-xfer-deny]").forEach((el) => {
    el.onclick = () => answer(el.dataset["xferDeny"]!, false);
  });
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
