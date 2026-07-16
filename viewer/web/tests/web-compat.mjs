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
execSync(
  `npx esbuild src/filerecv.ts --bundle --format=esm --outfile=/tmp/ndsp-filerecv-bundle.mjs --log-level=error`,
  { cwd: webRoot, stdio: "inherit" },
);
const { FileReceiver } = await import("/tmp/ndsp-filerecv-bundle.mjs");

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
let audioFrames = [];
let closed = null;
let controlHook = null; // extra per-test consumer (file receive)
const events = {
  onVideo: (f) => frames.push(f),
  onAudio: (f) => audioFrames.push(f),
  onControl: (m) => {
    if (controlHook?.(m)) return;
    controls.push(m);
  },
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

// ---- 1b. audio channel: opt in (PCM — decodable without WebCodecs), verify
// framing, then opt out and verify the stream stops -------------------------
await s1.send({ type: "set_audio", enabled: true, codec: "pcm" });
for (let i = 0; i < 100 && audioFrames.length < 10; i++) await sleep(100);
if (audioFrames.length < 10) fail(`only ${audioFrames.length} audio frames received`);
const a0 = audioFrames[0];
if (a0.codec !== "pcm_s16le") fail(`unexpected audio codec ${a0.codec}`);
if (a0.sampleRate !== 48000 || a0.channels !== 2) fail("audio format contract violated");
if (a0.payload.length !== 480 * 2 * 2) fail(`bad PCM block size ${a0.payload.length}`);
if (!audioFrames.slice(0, 10).every((f, i, arr) => i === 0 || f.seq > arr[i - 1].seq))
  fail("audio seq must increase");
// The test tone must be non-silent.
if (!audioFrames.some((f) => f.payload.some((b) => b !== 0))) fail("audio is all silence");
await s1.send({ type: "set_audio", enabled: false });
await sleep(400);
const countAfterOff = audioFrames.length;
await sleep(500);
if (audioFrames.length > countAfterOff + 2) fail("audio kept flowing after disable");
console.log(`audio channel OK (${countAfterOff} PCM frames, off-switch works)`);

// ---- 1c. host→viewer file send: panel upload → explicit viewer accept →
// chunked stream → sha256-verified file, using the REAL FileReceiver code ----
{
  const sent = new Uint8Array(600_123);
  for (let i = 0; i < sent.length; i += 65536) {
    crypto.getRandomValues(sent.subarray(i, Math.min(i + 65536, sent.length)));
  }
  let offered = null;
  let gotFile = null;
  let recvFail = null;
  const receiver = new FileReceiver(s1, {
    confirm: async (name, size) => {
      offered = { name, size };
      return true; // the "user" accepts
    },
    onProgress: () => {},
    onFile: (name, blob) => (gotFile = { name, blob }),
    onFail: (r) => (recvFail = r),
  });
  controlHook = (m) => receiver.handleControl(m);

  const panel = "http://127.0.0.1:41998";
  const status = await (await fetch(`${panel}/api/status`)).json();
  if (status.clients.length !== 1) fail(`panel sees ${status.clients.length} clients`);
  const res = await fetch(
    `${panel}/api/send-file?client_id=${status.clients[0].id}&name=compat.bin`,
    { method: "POST", body: sent },
  );
  if (!res.ok) fail(`send-file API: ${res.status} ${await res.text()}`);

  for (let i = 0; i < 200 && !gotFile && !recvFail; i++) await sleep(100);
  if (recvFail) fail(`file receive failed: ${recvFail}`);
  if (!gotFile) fail("file never arrived");
  if (!offered || offered.name !== "compat.bin" || offered.size !== sent.length)
    fail(`bad offer metadata: ${JSON.stringify(offered)}`);
  if (gotFile.name !== "compat.bin") fail(`bad received name ${gotFile.name}`);
  const got = new Uint8Array(await gotFile.blob.arrayBuffer());
  if (got.length !== sent.length) fail(`received ${got.length} of ${sent.length} bytes`);
  for (let i = 0; i < got.length; i++) {
    if (got[i] !== sent[i]) fail(`received file differs at byte ${i}`);
  }
  controlHook = null;
  console.log(`host→viewer file send OK (${sent.length} bytes, sha256 verified by the receiver)`);
}

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
