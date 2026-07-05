/**
 * Host control panel: status dashboard, pairing (PIN + QR), device trust
 * management, settings, and connection diagnostics.
 *
 * Talks to the loopback-only admin API; if opened from another machine the
 * API returns 403 and we show a hint instead.
 */

import QRCode from "qrcode";

const $ = <T extends HTMLElement>(id: string): T => document.getElementById(id) as T;

interface ClientDiag {
  device_id: string;
  name: string;
  remote_addr: string;
  streaming: boolean;
  fps: number;
  bitrate_kbps: number;
  rtt_ms: number;
  quality: number;
  width: number;
  height: number;
  frames_dropped: number;
}

interface AdminStatus {
  host_name: string;
  port: number;
  tls: boolean;
  tls_fingerprint: string | null;
  uptime_secs: number;
  frame_source: string;
  driver: { virtual_display_driver: string; capture_fallback: string; mode: string };
  audio_unavailable_reason: string | null;
  audio_enabled: boolean;
  clipboard_enabled: boolean;
  discovery: boolean;
  clients: ClientDiag[];
  lan_ips: string[];
  active_pin: { pin: string; expires_in_secs: number } | null;
}

interface DeviceEntry {
  device_id: string;
  name: string;
  input_allowed: boolean;
  last_seen_unix: number;
}

let lastStatus: AdminStatus | null = null;

async function api<T>(path: string, init?: RequestInit): Promise<T> {
  const res = await fetch(path, init);
  if (res.status === 403) {
    $("not-local").hidden = false;
    throw new Error("admin API forbidden (not localhost)");
  }
  if (!res.ok) throw new Error(`${path}: HTTP ${res.status}`);
  return res.status === 204 ? (undefined as T) : ((await res.json()) as T);
}

function fmtUptime(s: number): string {
  const h = Math.floor(s / 3600);
  const m = Math.floor((s % 3600) / 60);
  return h > 0 ? `${h}h ${m}m` : `${m}m ${s % 60}s`;
}

function driverLabel(d: AdminStatus["driver"]): string {
  switch (d.virtual_display_driver) {
    case "running": return "✅ virtual display driver active (extend + mirror)";
    case "not_detected": return "🟡 driver not detected — mirror mode via desktop duplication";
    case "windows_only": return "🧪 non-Windows host — test pattern (demo mode)";
    default: return d.virtual_display_driver;
  }
}

async function refreshStatus(): Promise<void> {
  const st = await api<AdminStatus>("/api/admin/status");
  lastStatus = st;
  $("host-title").textContent = st.host_name;

  const scheme = st.tls ? "https" : "http";
  const ip = st.lan_ips[0] ?? "<host-ip>";
  const kv = $("status-kv");
  kv.innerHTML = "";
  const add = (k: string, v: string) => {
    const dt = document.createElement("dt");
    dt.textContent = k;
    const dd = document.createElement("dd");
    dd.textContent = v;
    kv.append(dt, dd);
  };
  add("Viewer URL", `${scheme}://${ip}:${st.port}/view/`);
  add("Uptime", fmtUptime(st.uptime_secs));
  add("Capture", st.frame_source);
  add("Driver", driverLabel(st.driver));
  add("Transport", st.tls ? `TLS ✔ (${(st.tls_fingerprint ?? "").slice(0, 23)}…)` : "⚠ plain (testing only)");
  add("Audio", st.audio_unavailable_reason ? `unavailable — ${st.audio_unavailable_reason}` : st.audio_enabled ? "enabled" : "off (default)");
  add("Clients", String(st.clients.length));

  ($("set-audio") as HTMLInputElement).checked = st.audio_enabled;
  ($("set-clipboard") as HTMLInputElement).checked = st.clipboard_enabled;
  ($("set-discovery") as HTMLInputElement).checked = st.discovery;

  renderDiagnostics(st);
  await refreshDevices(st);
}

async function refreshDevices(st: AdminStatus): Promise<void> {
  const { devices } = await api<{ devices: DeviceEntry[] }>("/api/admin/devices");
  const body = $("devices-body");
  body.innerHTML = "";
  $("no-devices").hidden = devices.length > 0;

  for (const d of devices) {
    const live = st.clients.find((c) => c.device_id === d.device_id && c.streaming);
    const tr = document.createElement("tr");

    const name = document.createElement("td");
    name.textContent = d.name;
    const status = document.createElement("td");
    status.innerHTML = live
      ? `<span class="badge on">streaming</span>`
      : `<span class="badge off">offline</span>`;

    const input = document.createElement("td");
    const toggle = document.createElement("input");
    toggle.type = "checkbox";
    toggle.checked = d.input_allowed;
    toggle.title = "Allow this device to control mouse/keyboard";
    toggle.addEventListener("change", async () => {
      await api(`/api/admin/devices/${encodeURIComponent(d.device_id)}/input`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ allowed: toggle.checked }),
      });
    });
    input.append(toggle);

    const liveTd = document.createElement("td");
    liveTd.className = "muted";
    liveTd.textContent = live
      ? `${live.width}×${live.height} · ${live.fps.toFixed(0)}fps · ${(live.bitrate_kbps / 1000).toFixed(1)}Mbps · ${live.rtt_ms.toFixed(0)}ms`
      : "—";

    const actions = document.createElement("td");
    const revoke = document.createElement("button");
    revoke.textContent = "Revoke";
    revoke.className = "danger";
    revoke.addEventListener("click", async () => {
      if (confirm(`Revoke access for "${d.name}"? The device must pair again.`)) {
        await api(`/api/admin/devices/${encodeURIComponent(d.device_id)}`, { method: "DELETE" });
        void refreshStatus();
      }
    });
    actions.append(revoke);

    tr.append(name, status, input, liveTd, actions);
    body.append(tr);
  }
}

function renderDiagnostics(st: AdminStatus): void {
  const list = $("diag-list");
  const items: string[] = [];
  if (st.lan_ips.length === 0) {
    items.push("⚠ No LAN IP detected — check that the host is connected to a network.");
  }
  if (!st.tls) {
    items.push("⚠ TLS is off. Enable it (default) for encrypted streaming.");
  }
  items.push(
    `Firewall: allow inbound TCP ${st.port} and UDP 38471 for “NebulaDisplay Host” (private networks only).`,
  );
  if (st.driver.virtual_display_driver === "not_detected") {
    items.push(
      "Extend mode needs the virtual display driver: run installer/windows/install-driver.ps1 as admin (see docs/DRIVER.md).",
    );
  }
  items.push("Viewer can't connect? Try the host's IP directly, e.g. " +
    `${st.tls ? "https" : "http"}://${st.lan_ips[0] ?? "192.168.x.x"}:${st.port}/view/ ` +
    "and accept the certificate prompt on first visit.");
  items.push("Choppy stream on Wi-Fi? Switch the profile to Office, or move host/viewer to 5GHz/Ethernet.");
  list.innerHTML = "";
  for (const i of items) {
    const li = document.createElement("li");
    li.textContent = i;
    list.append(li);
  }
}

// Pairing.
$("pin-btn").addEventListener("click", async () => {
  const res = await api<{ pin: string; expires_in_secs: number; qr_payload: string }>(
    "/api/admin/pin",
    { method: "POST" },
  );
  $("pin-area").hidden = false;
  $("pin-value").textContent = res.pin;
  const st = lastStatus;
  const scheme = st?.tls ? "https" : "http";
  const ip = st?.lan_ips[0] ?? location.hostname;
  const url = `${scheme}://${ip}:${st?.port ?? location.port}/view/#host=${ip}:${st?.port}&pin=${res.pin}&autoconnect=1`;
  $("viewer-url").textContent = url;
  await QRCode.toCanvas($("qr-canvas") as HTMLCanvasElement, url, { width: 196, margin: 1 });

  let ttl = res.expires_in_secs;
  $("pin-ttl").textContent = String(ttl);
  const t = setInterval(() => {
    ttl -= 1;
    $("pin-ttl").textContent = String(Math.max(0, ttl));
    if (ttl <= 0) {
      clearInterval(t);
      $("pin-area").hidden = true;
    }
  }, 1000);
});

// Settings.
for (const [id, key] of [
  ["set-audio", "audio_enabled"],
  ["set-clipboard", "clipboard_enabled"],
  ["set-discovery", "discovery"],
] as const) {
  $(id).addEventListener("change", async (e) => {
    const checked = (e.target as HTMLInputElement).checked;
    await api("/api/admin/config", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ [key]: checked }),
    });
  });
}

// Poll status every 2s.
void refreshStatus().catch((e) => console.warn(e));
setInterval(() => void refreshStatus().catch(() => {}), 2000);
