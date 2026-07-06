#!/usr/bin/env node
// Full browser E2E: real nebulad host + real Chromium running the built web
// viewer. Verifies: UI pairing with PIN → H.264 WebCodecs decode → canvas
// pixels actually change → stats overlay shows measured latency → input
// events reach the host (log sink on non-Windows) → control panel renders
// PIN/QR/clients. Saves screenshots for human review.
//
// Usage: node tests/browser-e2e.mjs [screenshot-dir]

import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const webRoot = join(here, "..");
const repoRoot = join(webRoot, "..", "..");
const shotDir = process.argv[2] ?? "/tmp/ndsp-shots";
mkdirSync(shotDir, { recursive: true });

const dataDir = mkdtempSync(join(tmpdir(), "ndsp-browser-e2e-"));
const port = 41997;
const panelPort = 41996;

const host = spawn(
  process.env.NEBULAD_BIN ??
    (existsSync(join(repoRoot, "target", "release", "nebulad"))
      ? join(repoRoot, "target", "release", "nebulad")
      : join(repoRoot, "target", "debug", "nebulad")),
  [
    "--test-pattern", "--bind", "127.0.0.1",
    "--port", String(port), "--panel-port", String(panelPort),
    "--discovery-port", "0",
    "--data-dir", dataDir,
    "--capture-size", "1280x720",
    "--name", "Browser E2E Host",
    "--web-dir", join(webRoot, "dist"),
  ],
  { stdio: ["ignore", "pipe", "pipe"] },
);
let pin = null;
let hostLog = "";
host.stdout.on("data", (d) => {
  hostLog += d.toString();
  const m = hostLog.match(/PIN \(single-use\)[^\n]*\n\s+(\d{4,10})/);
  if (m) pin = m[1];
});
host.stderr.on("data", (d) => (hostLog += d.toString()));

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
async function fail(msg) {
  console.error(`FAIL: ${msg}`);
  console.error("--- host log tail ---\n" + hostLog.split("\n").slice(-25).join("\n"));
  host.kill();
  process.exit(1);
}

for (let i = 0; i < 100 && pin === null; i++) await sleep(100);
if (!pin) await fail("no PIN from host");
console.log(`host up on :${port}, pin=${pin}`);

const browser = await chromium.launch({
  executablePath: process.env.CHROMIUM_PATH || undefined,
  args: [
    "--autoplay-policy=no-user-gesture-required",
    ...(process.getuid?.() === 0 ? ["--no-sandbox"] : []),
  ],
});
const page = await browser.newPage({ viewport: { width: 1400, height: 900 } });
page.on("console", (m) => {
  if (m.type() === "error") console.log(`[browser] ${m.text()}`);
});
page.on("pageerror", (e) => console.log(`[pageerror] ${e.message}`));

// ---- pair through the actual UI --------------------------------------------
await page.goto(`http://127.0.0.1:${port}/`);
await page.fill("#host", `127.0.0.1:${port}`);
await page.fill("#pin", pin);
await page.fill("#client-name", "Chromium E2E");
await page.screenshot({ path: join(shotDir, "1-connect.png") });
await page.click("#connect-btn");
await page.waitForSelector("#viewer-screen.active", { timeout: 15000 }).catch(() => fail("viewer did not activate"));
console.log("paired through UI");

// ---- verify streaming with changing pixels ----------------------------------
// The negotiated codec must match the browser's REAL decode capability:
// H.264 with proprietary-codec Chromium builds, JPEG otherwise.
const h264Capable = await page.evaluate(
  async () =>
    "VideoDecoder" in globalThis &&
    (await VideoDecoder.isConfigSupported({ codec: "avc1.42E01F" })).supported === true,
);
await sleep(2500);
const probe = async () =>
  page.evaluate(() => {
    const c = document.getElementById("screen");
    const ctx = c.getContext("2d");
    const d = ctx.getImageData(0, 0, Math.min(64, c.width), 8).data;
    let sum = 0;
    for (let i = 0; i < d.length; i += 97) sum = (sum * 31 + d[i]) >>> 0;
    return { w: c.width, h: c.height, hash: sum, name: document.getElementById("server-name").textContent };
  });
let p1 = await probe();
for (let i = 0; i < 40 && (p1.w !== 1280 || p1.h !== 720); i++) {
  await sleep(250);
  p1 = await probe();
}
if (p1.w !== 1280 || p1.h !== 720) await fail(`canvas is ${p1.w}x${p1.h}, expected 1280x720`);
const wantCodec = h264Capable ? /H264/i : /JPEG/i;
if (!wantCodec.test(p1.name))
  await fail(`expected ${h264Capable ? "H264" : "JPEG"} codec badge (h264 decodable=${h264Capable}), got "${p1.name}"`);
await sleep(700);
const p2 = await probe();
if (p1.hash === p2.hash) await fail("canvas pixels not changing — stream frozen?");
console.log(`${h264Capable ? "H.264" : "JPEG"} streaming verified: 1280x720, pixels changing (${p1.hash} → ${p2.hash})`);

// ---- stats overlay with measured e2e latency --------------------------------
await page.click("#stats-btn");
await sleep(2600);
const stats = await page.textContent("#stats-overlay");
console.log("overlay:\n" + stats.split("\n").map((l) => "   " + l).join("\n"));
const fpsMatch = stats.match(/decode fps\s+([\d.]+)/);
const e2eMatch = stats.match(/e2e\s+([\d.]+) ms/);
if (!fpsMatch || parseFloat(fpsMatch[1]) < 5) await fail(`decode fps too low: ${stats}`);
if (!e2eMatch) await fail("no measured e2e latency in overlay");
const e2e = parseFloat(e2eMatch[1]);
if (e2e <= 0 || e2e > 5000) await fail(`implausible e2e latency ${e2e}`);
console.log(`measured e2e latency: ${e2e} ms @ ${fpsMatch[1]} fps decoded`);
await page.screenshot({ path: join(shotDir, "2-streaming.png") });

// ---- input path: select keyboard_mouse, move mouse, expect host log --------
await page.selectOption("#input-mode", "keyboard_mouse");
await sleep(300);
const denied = await page.isVisible("#input-denied");
if (!denied) await fail("input-denied badge should show before grant");
// Grant input via panel API (loopback).
const deviceId = await page.evaluate(() => localStorage.getItem("ndsp.deviceId"));
const res = await fetch(`http://127.0.0.1:${panelPort}/api/grant`, {
  method: "POST",
  headers: { "content-type": "application/json" },
  body: JSON.stringify({ device_id: deviceId, allowed: true }),
});
if (!res.ok) await fail(`grant API failed: ${res.status}`);
await sleep(500);
if (await page.isVisible("#input-denied")) await fail("denied badge should clear after grant");
const canvasBox = await page.locator("#screen").boundingBox();
await page.mouse.move(canvasBox.x + canvasBox.width / 2, canvasBox.y + canvasBox.height / 2);
await page.mouse.down();
await page.mouse.move(canvasBox.x + canvasBox.width / 2 + 80, canvasBox.y + canvasBox.height / 2 + 40, { steps: 5 });
await page.mouse.up();
await page.keyboard.press("KeyN");
await sleep(800);
if (!/input event/.test(hostLog)) await fail("host did not log injected input events");
console.log("input path verified (host received mouse/key events after grant)");

// ---- control panel ----------------------------------------------------------
const panel = await browser.newPage({ viewport: { width: 1200, height: 900 } });
await panel.goto(`http://127.0.0.1:${panelPort}/panel.html`);
await sleep(1500);
const pinShown = await panel.textContent("#pin");
if (!/^\d{4,10}$/.test(pinShown.trim())) await fail(`panel PIN malformed: "${pinShown}"`);
const qrOk = await panel.evaluate(async () => {
  const r = await fetch("/api/qr.svg");
  return r.ok && (await r.text()).includes("<svg");
});
if (!qrOk) await fail("QR endpoint broken");
const clientRow = await panel.textContent("#clients tbody");
if (!clientRow.includes("Chromium E2E")) await fail("connected client missing from panel");
if (!clientRow.includes("granted")) await fail("grant state not reflected in panel");
await panel.screenshot({ path: join(shotDir, "3-panel.png"), fullPage: true });
console.log("panel verified (PIN, QR, live client with grant)");

// ---- profile switch + fullscreen button exist -------------------------------
await page.selectOption("#profile", "gaming");
await sleep(1200);
const p3 = await probe();
await sleep(500);
const p4 = await probe();
if (p3.hash === p4.hash) await fail("stream stalled after profile switch");
console.log("profile switch to gaming OK, stream continues");

// ---- cursor channel: the host cursor renders as a moving overlay -----------
// The test-pattern source emits a synthetic cursor circling the center, so a
// cursor-capable viewer must show #remote-cursor with a changing transform.
const cur1 = await page.evaluate(() => {
  const el = document.getElementById("remote-cursor");
  return el ? { display: getComputedStyle(el).display, t: el.style.transform, src: !!el.src } : null;
});
await sleep(700);
const cur2 = await page.evaluate(() => {
  const el = document.getElementById("remote-cursor");
  return el ? { display: getComputedStyle(el).display, t: el.style.transform, src: !!el.src } : null;
});
if (!cur1 || cur1.display === "none" || !cur1.src) await fail("remote cursor overlay not visible");
if (cur1.t === cur2.t) await fail("remote cursor is not moving (cursor channel stalled)");
console.log("cursor channel verified (shape delivered, position updating)");

await browser.close();
host.kill();
rmSync(dataDir, { recursive: true, force: true });
console.log(`\nPASS: full browser E2E (screenshots in ${shotDir})`);
process.exit(0);
