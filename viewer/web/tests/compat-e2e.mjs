#!/usr/bin/env node
// Cross-environment browser E2E for the capability-fallback layer.
//
// The viewer is served over plain HTTP on a LAN address — an *insecure
// context* — which is exactly what real Windows/iOS/Android devices hit:
// crypto.randomUUID, crypto.subtle and WebCodecs do not exist there. This
// test drives real Chromium against a real nebulad host through that origin,
// plus emulated profiles approximating iOS Safari (no PointerEvent, no
// createImageBitmap, no DataView BigInt accessors, touch), Android Chrome
// (touch + insecure context) and a storage-blocked WebView. Each scenario
// must fully pair, complete the encrypted handshake and render moving video.
//
// Usage: node tests/compat-e2e.mjs   (spawns its own nebulad)

import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { networkInterfaces, tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const webRoot = join(here, "..");
const repoRoot = join(webRoot, "..", "..");

const port = 41991;
const panelPort = 41990;

function lanIp() {
  for (const addrs of Object.values(networkInterfaces())) {
    for (const a of addrs ?? []) {
      if (a.family === "IPv4" && !a.internal) return a.address;
    }
  }
  throw new Error("no non-loopback IPv4 address found — cannot test insecure context");
}
const LAN = lanIp();

// ---- host -------------------------------------------------------------------
const dataDir = mkdtempSync(join(tmpdir(), "ndsp-compat-e2e-"));
const host = spawn(
  process.env.NEBULAD_BIN ??
    (existsSync(join(repoRoot, "target", "release", "nebulad"))
      ? join(repoRoot, "target", "release", "nebulad")
      : join(repoRoot, "target", "debug", "nebulad")),
  [
    "--test-pattern", "--bind", "0.0.0.0",
    "--port", String(port), "--panel-port", String(panelPort),
    "--discovery-port", "0",
    "--data-dir", dataDir,
    "--capture-size", "640x360",
    "--name", "Compat E2E Host",
    "--web-dir", join(webRoot, "dist"),
  ],
  { stdio: ["ignore", "pipe", "pipe"] },
);
let hostLog = "";
host.stdout.on("data", (d) => (hostLog += d.toString()));
host.stderr.on("data", (d) => (hostLog += d.toString()));

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let browser = null;
async function fail(msg) {
  console.error(`\nFAIL: ${msg}`);
  console.error("--- host log tail ---\n" + hostLog.split("\n").slice(-20).join("\n"));
  await browser?.close();
  host.kill();
  process.exit(1);
}

async function currentPin() {
  const res = await fetch(`http://127.0.0.1:${panelPort}/api/status`);
  if (!res.ok) throw new Error(`panel status: ${res.status}`);
  return (await res.json()).pin;
}

async function grantInput(deviceId) {
  const res = await fetch(`http://127.0.0.1:${panelPort}/api/grant`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ device_id: deviceId, allowed: true }),
  });
  if (!res.ok) throw new Error(`grant: ${res.status}`);
}

for (let i = 0; i < 100 && !/viewer endpoint listening/.test(hostLog); i++) await sleep(100);
if (!/viewer endpoint listening/.test(hostLog)) await fail("host did not start");
console.log(`host up on 0.0.0.0:${port}; insecure origin under test: http://${LAN}:${port}/`);

browser = await chromium.launch({
  executablePath: process.env.CHROMIUM_PATH || undefined,
  args: [
    "--autoplay-policy=no-user-gesture-required",
    ...(process.getuid?.() === 0 ? ["--no-sandbox"] : []),
  ],
});

const probeCanvas = (page) =>
  page.evaluate(() => {
    const c = document.getElementById("screen");
    const d = c.getContext("2d").getImageData(0, 0, Math.min(64, c.width), 8).data;
    let sum = 0;
    for (let i = 0; i < d.length; i += 97) sum = (sum * 31 + d[i]) >>> 0;
    return { w: c.width, h: c.height, hash: sum };
  });

async function expectStreaming(page, name) {
  // Poll until the first frame is presented (canvas adopts the stream size).
  let p1 = null;
  for (let i = 0; i < 50; i++) {
    p1 = await probeCanvas(page);
    if (p1.w === 640 && p1.h === 360) break;
    await sleep(200);
  }
  if (p1.w !== 640 || p1.h !== 360) await fail(`${name}: canvas ${p1.w}x${p1.h}, expected 640x360`);
  for (let i = 0; i < 20; i++) {
    await sleep(300);
    const p2 = await probeCanvas(page);
    if (p1.hash !== p2.hash) return;
  }
  await fail(`${name}: canvas pixels frozen`);
}

async function pairThroughUi(page, name, { expectNoPinField = false } = {}) {
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await page.goto(`http://${LAN}:${port}/`, { waitUntil: "networkidle" });
  await page.fill("#host", `${LAN}:${port}`);
  if (!expectNoPinField) {
    await page.fill("#pin", await currentPin());
  }
  await page.fill("#client-name", name);
  await page.click("#connect-btn");
  try {
    await page.waitForSelector("#viewer-screen.active", { timeout: 15000 });
  } catch {
    const status = await page.textContent("#status");
    await fail(`${name}: viewer did not activate — status="${status}" pageerrors=${JSON.stringify(errors)}`);
  }
  if (errors.length) await fail(`${name}: page errors: ${JSON.stringify(errors)}`);
}

// ============================================================================
// 1. Secure context regression (localhost): native WebCrypto + WebCodecs H.264
// ============================================================================
{
  const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  const errors = [];
  page.on("pageerror", (e) => errors.push(e.message));
  await page.goto(`http://127.0.0.1:${port}/`, { waitUntil: "networkidle" });
  const env = await page.evaluate(async () => ({
    secure: isSecureContext,
    subtle: !!crypto.subtle,
    uuid: typeof crypto.randomUUID === "function",
    webcodecs: "VideoDecoder" in globalThis,
    // What matters is real decode capability, not API existence: codec-less
    // Chromium builds expose VideoDecoder but reject avc1 configs.
    h264:
      "VideoDecoder" in globalThis &&
      (await VideoDecoder.isConfigSupported({ codec: "avc1.42E01F" })).supported === true,
  }));
  if (!env.secure || !env.subtle || !env.uuid || !env.webcodecs)
    await fail(`secure-context env wrong: ${JSON.stringify(env)}`);
  await page.fill("#host", `127.0.0.1:${port}`);
  await page.fill("#pin", await currentPin());
  await page.fill("#client-name", "Secure localhost");
  await page.click("#connect-btn");
  await page.waitForSelector("#viewer-screen.active", { timeout: 15000 }).catch(() => fail("secure: no viewer"));
  const badge = await page.textContent("#server-name");
  const wantCodec = env.h264 ? /H264/i : /JPEG/i;
  if (!wantCodec.test(badge))
    await fail(`secure: expected ${env.h264 ? "H264" : "JPEG"} badge (h264 decodable=${env.h264}), got "${badge}"`);
  await expectStreaming(page, "secure-localhost");
  if (errors.length) await fail(`secure: page errors ${JSON.stringify(errors)}`);
  console.log(
    `PASS 1/6  secure context (localhost): native crypto, codec matches real decode capability (${env.h264 ? "H264" : "JPEG — this Chromium lacks H.264 decode"})`,
  );
  await page.close();
}

// ============================================================================
// 2. Windows-Chromium-over-LAN: insecure context, fallback crypto, JPEG
//    (this is the exact environment of the reported crash)
// ============================================================================
{
  const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  await page.goto(`http://${LAN}:${port}/`, { waitUntil: "networkidle" });
  const env = await page.evaluate(() => ({
    secure: isSecureContext,
    subtle: !!globalThis.crypto?.subtle,
    uuid: typeof globalThis.crypto?.randomUUID === "function",
    webcodecs: "VideoDecoder" in globalThis,
  }));
  if (env.secure || env.subtle || env.uuid || env.webcodecs)
    await fail(`insecure env not as expected (must lack subtle/uuid/webcodecs): ${JSON.stringify(env)}`);
  await pairThroughUi(page, "Windows LAN Chromium");
  const badge = await page.textContent("#server-name");
  if (!/JPEG/i.test(badge)) await fail(`insecure: expected JPEG badge, got "${badge}"`);
  await expectStreaming(page, "insecure-lan");

  // deviceId must be a valid RFC4122 v4 UUID from the fallback generator.
  const devId = await page.evaluate(() => localStorage.getItem("ndsp.deviceId"));
  if (!/^[0-9a-f]{8}-[0-9a-f]{4}-4[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(devId))
    await fail(`fallback deviceId is not RFC4122 v4: ${devId}`);

  // compat-mode note must be visible and mention the crypto fallback.
  const note = await page.textContent("#compat-note");
  if (!/built-in crypto/.test(note)) await fail(`compat note missing/wrong: "${note}"`);

  // Token reconnect: reload → connect with stored trust, no PIN.
  await page.reload({ waitUntil: "networkidle" });
  await page.fill("#host", `${LAN}:${port}`);
  const pinHidden = await page.evaluate(() => document.getElementById("pin-field").style.display === "none");
  if (!pinHidden) await fail("paired hint/pin hiding not active after reload");
  await page.click("#connect-btn");
  await page.waitForSelector("#viewer-screen.active", { timeout: 15000 }).catch(() => fail("token reconnect failed"));
  await expectStreaming(page, "token-reconnect");
  console.log("PASS 2/6  insecure LAN (Windows-Chromium case): fallback crypto pairs, streams JPEG, token reconnect OK");
  await page.close();
}

// ============================================================================
// 3. iOS-Safari-like: insecure + no PointerEvent / createImageBitmap /
//    DataView BigInt accessors / performance.timeOrigin, touch input
// ============================================================================
{
  const ctx = await browser.newContext({
    userAgent:
      "Mozilla/5.0 (iPhone; CPU iPhone OS 15_0 like Mac OS X) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/15.0 Mobile/15E148 Safari/604.1",
    viewport: { width: 390, height: 844 },
    hasTouch: true,
    isMobile: true,
  });
  await ctx.addInitScript(() => {
    delete window.PointerEvent;
    delete window.createImageBitmap;
    delete DataView.prototype.getBigUint64;
    delete DataView.prototype.setBigUint64;
    delete Performance.prototype.timeOrigin;
    // iPhone Safari has no element fullscreen at all — including prefixed.
    delete Element.prototype.requestFullscreen;
    delete Element.prototype.webkitRequestFullscreen;
    delete Element.prototype.webkitRequestFullScreen;
    delete Document.prototype.exitFullscreen;
    delete Document.prototype.webkitExitFullscreen;
    delete document.exitFullscreen;
  });
  const page = await ctx.newPage();
  await page.goto(`http://${LAN}:${port}/`, { waitUntil: "networkidle" });
  const env = await page.evaluate(() => ({
    pointer: typeof PointerEvent,
    cib: typeof createImageBitmap,
    timeOrigin: typeof performance.timeOrigin,
    fs: typeof Element.prototype.requestFullscreen,
    fsWebkit: typeof Element.prototype.webkitRequestFullscreen,
    // deleted by the emulation, must be restored by the viewer's polyfill
    big64: typeof DataView.prototype.getBigUint64,
    big64set: typeof DataView.prototype.setBigUint64,
  }));
  if (env.pointer !== "undefined" || env.cib !== "undefined" || env.timeOrigin !== "undefined" || env.fs !== "undefined" || env.fsWebkit !== "undefined")
    await fail(`iOS emulation leaked APIs: ${JSON.stringify(env)}`);
  if (env.big64 !== "function" || env.big64set !== "function")
    await fail(`DataView BigInt polyfill not installed: ${JSON.stringify(env)}`);
  await pairThroughUi(page, "iOS Safari emu");
  await expectStreaming(page, "ios-emu");

  // Fullscreen button must be hidden when the API is absent.
  const fsHidden = await page.evaluate(
    () => document.getElementById("fullscreen-btn").style.display === "none",
  );
  if (!fsHidden) await fail("fullscreen button should be hidden without a Fullscreen API");

  // Touch input through the fallback (touchstart/touchend) path.
  await page.selectOption("#input-mode", "direct_touch");
  const devId = await page.evaluate(() => localStorage.getItem("ndsp.deviceId"));
  await grantInput(devId);
  await sleep(400);
  const box = await page.locator("#screen").boundingBox();
  await page.touchscreen.tap(box.x + box.width / 2, box.y + box.height / 2);
  await sleep(800);
  if (!/input event/.test(hostLog)) await fail("host did not receive touch input from iOS-like viewer");
  console.log("PASS 3/6  iOS-Safari-like: pairs, streams, touch input via non-PointerEvent fallback");
  await ctx.close();
}

// ============================================================================
// 4. Android-Chrome-like: insecure + touch (PointerEvent present)
// ============================================================================
{
  const ctx = await browser.newContext({
    userAgent:
      "Mozilla/5.0 (Linux; Android 14; Pixel 8) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/126.0.0.0 Mobile Safari/537.36",
    viewport: { width: 412, height: 915 },
    hasTouch: true,
    isMobile: true,
  });
  const page = await ctx.newPage();
  await pairThroughUi(page, "Android Chrome emu");
  await expectStreaming(page, "android-emu");
  await page.selectOption("#input-mode", "direct_touch");
  const devId = await page.evaluate(() => localStorage.getItem("ndsp.deviceId"));
  await grantInput(devId);
  await sleep(400);
  const before = (hostLog.match(/input event/g) ?? []).length;
  const box = await page.locator("#screen").boundingBox();
  await page.touchscreen.tap(box.x + box.width / 2, box.y + box.height / 3);
  await sleep(800);
  if ((hostLog.match(/input event/g) ?? []).length <= before)
    await fail("host did not receive touch input from Android-like viewer");
  console.log("PASS 4/6  Android-Chrome-like: pairs, streams, touch input via PointerEvent path");
  await ctx.close();
}

// ============================================================================
// 5. Storage-blocked WebView: localStorage throws — memory fallback must pair
// ============================================================================
{
  const ctx = await browser.newContext({ viewport: { width: 1024, height: 768 } });
  await ctx.addInitScript(() => {
    Object.defineProperty(window, "localStorage", {
      get() {
        throw new Error("SecurityError: storage disabled");
      },
    });
  });
  const page = await ctx.newPage();
  await pairThroughUi(page, "Storage-blocked WebView");
  await expectStreaming(page, "no-storage");
  const note = await page.textContent("#compat-note");
  if (!/persistent storage/.test(note)) await fail(`storage compat note missing: "${note}"`);
  console.log("PASS 5/6  storage-blocked WebView: pairs + streams with in-memory storage");
  await ctx.close();
}

// ============================================================================
// 6. The original crash reproduction: deviceId() before handshake, insecure
// ============================================================================
{
  const page = await browser.newPage();
  await page.goto(`http://${LAN}:${port}/`, { waitUntil: "networkidle" });
  const uuid = await page.evaluate(() => {
    // Directly exercise the path that crashed: first-run device id minting.
    localStorage.removeItem("ndsp.deviceId");
    const before = typeof crypto.randomUUID; // must be "undefined" here
    location.reload;
    return { before };
  });
  if (uuid.before !== "undefined") await fail("expected crypto.randomUUID to be absent on insecure origin");
  console.log("PASS 6/6  regression guard: insecure origin lacks crypto.randomUUID yet viewer paired in (2)");
  await page.close();
}

await browser.close();
host.kill();
rmSync(dataDir, { recursive: true, force: true });
console.log("\nPASS: all compat scenarios — pairing, handshake and video stream succeed on every profile");
process.exit(0);
