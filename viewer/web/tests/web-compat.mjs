#!/usr/bin/env node
// Cross-stack compatibility test: runs the REAL web-viewer session code
// (src/session.ts, bundled by esbuild) under Node 22's WebCrypto + WebSocket
// against a REAL nebulad host. Verifies the browser crypto path is
// byte-compatible with the Rust implementation end-to-end:
//   pair(PIN) → AES-GCM envelopes → receive+parse video → token reconnect.
//
// Usage: node tests/web-compat.mjs  (spawns its own nebulad)

import { spawn, execSync } from "node:child_process";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const webRoot = join(here, "..");
const repoRoot = join(webRoot, "..", "..");

// ---- polyfills for browser globals used by the viewer code ----------------
// NDSP_CRYPTO=fallback strips SubtleCrypto/randomUUID (leaving only
// getRandomValues) to mirror an insecure browser context, proving the
// pure-JS fallback backend is byte-compatible with the Rust host too.
if (process.env.NDSP_CRYPTO === "fallback") {
  const grv = crypto.getRandomValues.bind(crypto);
  Object.defineProperty(globalThis, "crypto", {
    value: { getRandomValues: grv },
    configurable: true,
  });
  console.log("crypto backend: pure-JS fallback (SubtleCrypto removed)");
} else {
  console.log("crypto backend: native WebCrypto");
}
const store = new Map();
globalThis.localStorage = {
  getItem: (k) => (store.has(k) ? store.get(k) : null),
  setItem: (k, v) => store.set(k, String(v)),
  removeItem: (k) => store.delete(k),
};
globalThis.location = { search: "", host: "", hostname: "", protocol: "http:" };

// ---- bundle the real session module ---------------------------------------
execSync(
  `npx esbuild src/session.ts --bundle --format=esm --outfile=/tmp/ndsp-session-bundle.mjs --log-level=error`,
  { cwd: webRoot, stdio: "inherit" },
);
const { Session } = await import("/tmp/ndsp-session-bundle.mjs");

// ---- start a real host -----------------------------------------------------
const dataDir = mkdtempSync(join(tmpdir(), "ndsp-webcompat-"));
const port = 41999;
const host = spawn(
  process.env.NEBULAD_BIN ??
    (existsSync(join(repoRoot, "target", "release", "nebulad"))
      ? join(repoRoot, "target", "release", "nebulad")
      : join(repoRoot, "target", "debug", "nebulad")),
  [
    "--test-pattern",
    "--port", String(port),
    "--panel-port", "41998",
    "--discovery-port", "0",
    "--bind", "127.0.0.1",
    "--data-dir", dataDir,
    "--capture-size", "320x240",
    "--name", "webcompat-host",
  ],
  { stdio: ["ignore", "pipe", "pipe"] },
);
let pin = null;
let stdoutBuf = "";
host.stdout.on("data", (d) => {
  stdoutBuf += d.toString();
  const m = stdoutBuf.match(/PIN \(single-use\)[^\n]*\n\s+(\d{4,10})/);
  if (m) pin = m[1];
});
host.stderr.on("data", () => {});

function fail(msg) {
  console.error(`FAIL: ${msg}`);
  host.kill();
  rmSync(dataDir, { recursive: true, force: true });
  process.exit(1);
}
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// Wait for host + PIN.
for (let i = 0; i < 100 && pin === null; i++) await sleep(100);
if (!pin) fail("host did not print a PIN");
console.log(`host up, pin=${pin}`);

// ---- 1. pair with PIN using the real web session code ----------------------
let frames = [];
let controls = [];
let closed = null;
const events = {
  onVideo: (f) => frames.push(f),
  onControl: (m) => controls.push(m),
  onClose: (r) => (closed = r),
};
const hostAddr = `127.0.0.1:${port}`;
const s1 = await Session.connect(hostAddr, pin, "Node compat tester", events).catch((e) =>
  fail(`pairing failed: ${e.message}`),
);
console.log(`paired: codec=${s1.info.codec} mode=${s1.info.mode.width}x${s1.info.mode.height} newlyPaired=${s1.info.newlyPaired}`);
if (!s1.info.newlyPaired) fail("expected newlyPaired=true");
if (s1.info.inputAllowed) fail("input must be denied by default");

// Clock-sync ping through the encrypted channel.
await s1.send({ type: "ping", t0_us: 424242 });

// Collect frames.
for (let i = 0; i < 100 && frames.length < 5; i++) await sleep(100);
if (frames.length < 5) fail(`only ${frames.length} frames received`);
const f0 = frames[0];
console.log(`frames: ${frames.length}, first: codec=${f0.codec} key=${f0.keyframe} ${f0.width}x${f0.height} ${f0.payload.length}B`);
if (!f0.keyframe) fail("first frame must be keyframe");
if (f0.codec === "jpeg" && !(f0.payload[0] === 0xff && f0.payload[1] === 0xd8)) fail("bad JPEG magic");
if (f0.codec === "h264" && !findAnnexB(f0.payload)) fail("no Annex-B start code");
const pong = controls.find((c) => c.type === "pong");
if (!pong || pong.t0_us !== 424242) fail("no matching pong");
console.log("encrypted ping/pong OK");
if (new Set(frames.map((f) => f.seq)).size !== frames.length) fail("duplicate seq");

s1.close();
await sleep(300);

// ---- 2. token reconnect (no PIN) -------------------------------------------
frames = [];
controls = [];
const s2 = await Session.connect(hostAddr, null, "Node compat tester", events).catch((e) =>
  fail(`token reconnect failed: ${e.message}`),
);
if (s2.info.newlyPaired) fail("reconnect must not re-pair");
for (let i = 0; i < 100 && frames.length < 2; i++) await sleep(100);
if (frames.length < 2) fail("no frames after reconnect");
console.log("token reconnect OK, frames flowing");
s2.close();
await sleep(300);

// ---- 2b. clipboard sync (grant → push → fan-out to the other viewer) -------
{
  const deviceId = store.get("ndsp.deviceId");
  if (!deviceId) fail("no stored device id after pairing");

  const mkEvents = () => {
    const st = { clipboard: [], closed: null };
    st.events = {
      onVideo: () => {},
      onControl: (m) => {
        if (m.type === "clipboard") st.clipboard.push(m.text);
      },
      onClose: (r) => (st.closed = r),
    };
    return st;
  };
  const ea = mkEvents();
  const eb = mkEvents();
  const sa = await Session.connect(hostAddr, null, "Clip A", ea.events).catch((e) =>
    fail(`clip A reconnect failed: ${e.message}`),
  );
  if (sa.info.clipboardAllowed) fail("clipboard must be denied by default");

  // Grant via the loopback panel API (what the panel toggle calls).
  const res = await fetch("http://127.0.0.1:41998/api/clipboard-grant", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ device_id: deviceId, allowed: true }),
  });
  if (!res.ok) fail(`clipboard-grant API: ${res.status}`);

  const sb = await Session.connect(hostAddr, null, "Clip B", eb.events).catch((e) =>
    fail(`clip B reconnect failed: ${e.message}`),
  );
  if (!sb.info.clipboardAllowed) fail("grant must be reflected in auth_ok");

  await sa.send({ type: "clipboard", text: "cross-stack clipboard ✓" });
  for (let i = 0; i < 100 && eb.clipboard.length === 0; i++) await sleep(100);
  if (eb.clipboard[0] !== "cross-stack clipboard ✓") {
    fail(`clipboard fan-out failed: got ${JSON.stringify(eb.clipboard)}`);
  }
  if (ea.clipboard.length !== 0) fail("origin session must not receive its own clipboard echo");
  console.log("clipboard sync OK (grant, push, fan-out, no echo)");
  sa.close();
  sb.close();
  await sleep(300);
}

// ---- 3. wrong PIN must fail cleanly ----------------------------------------
store.clear(); // forget credentials → forces pairing path
const badPin = pin === "000000" ? "000001" : "000000";
let failedProperly = false;
try {
  await Session.connect(hostAddr, badPin, "Evil node", events);
} catch (e) {
  failedProperly = /pin/i.test(e.message);
  if (!failedProperly) fail(`wrong-PIN error message unexpected: ${e.message}`);
}
if (!failedProperly) fail("wrong PIN was accepted!");
console.log("wrong PIN rejected OK");

host.kill();
rmSync(dataDir, { recursive: true, force: true });
console.log("\nPASS: web viewer crypto/protocol is byte-compatible with the Rust host");
process.exit(0);

function findAnnexB(p) {
  for (let i = 0; i + 3 < p.length; i++) {
    if (p[i] === 0 && p[i + 1] === 0 && p[i + 2] === 0 && p[i + 3] === 1) return true;
  }
  return false;
}
