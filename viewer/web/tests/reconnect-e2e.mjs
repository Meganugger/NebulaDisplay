#!/usr/bin/env node
// Auto-reconnect E2E: real nebulad + real Chromium. Pairs through the UI,
// then SIGKILLs the host mid-stream (simulating a Wi-Fi drop / host restart)
// and restarts it on the same port + data dir. The viewer must recover the
// session BY ITSELF via token reconnect — no user interaction, no PIN.
//
// Usage: node tests/reconnect-e2e.mjs

import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const webRoot = join(here, "..");
const repoRoot = join(webRoot, "..", "..");

const dataDir = mkdtempSync(join(tmpdir(), "ndsp-reconnect-e2e-"));
const port = 41987;
const panelPort = 41986;
const bin =
  process.env.NEBULAD_BIN ??
  (existsSync(join(repoRoot, "target", "release", "nebulad"))
    ? join(repoRoot, "target", "release", "nebulad")
    : join(repoRoot, "target", "debug", "nebulad"));

let hostLog = "";
function startHost() {
  const h = spawn(
    bin,
    [
      "--test-pattern", "--bind", "127.0.0.1",
      "--port", String(port), "--panel-port", String(panelPort),
      "--discovery-port", "0",
      "--data-dir", dataDir,
      "--capture-size", "640x360",
      "--name", "Reconnect E2E Host",
      "--web-dir", join(webRoot, "dist"),
    ],
    { stdio: ["ignore", "pipe", "pipe"] },
  );
  h.stdout.on("data", (d) => (hostLog += d.toString()));
  h.stderr.on("data", (d) => (hostLog += d.toString()));
  return h;
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
let host = startHost();
let browser = null;
async function fail(msg) {
  console.error(`FAIL: ${msg}`);
  console.error("--- host log tail ---\n" + hostLog.split("\n").slice(-20).join("\n"));
  try { host.kill("SIGKILL"); } catch {}
  if (browser) await browser.close().catch(() => {});
  process.exit(1);
}

// Wait for the PIN banner (latest occurrence, in case of restarts).
async function currentPin() {
  for (let i = 0; i < 100; i++) {
    const all = [...hostLog.matchAll(/PIN \(single-use\)[^\n]*\n\s+(\d{4,10})/g)];
    if (all.length > 0) return all[all.length - 1][1];
    await sleep(100);
  }
  return null;
}

const pin = await currentPin();
if (!pin) await fail("no PIN from host");
console.log(`host up on :${port}, pin=${pin}`);

browser = await chromium.launch({
  executablePath: process.env.CHROMIUM_PATH || undefined,
  args: [...(process.getuid?.() === 0 ? ["--no-sandbox"] : [])],
});
const page = await browser.newPage({ viewport: { width: 900, height: 600 } });
page.on("pageerror", (e) => console.log(`[pageerror] ${e.message}`));

await page.goto(`http://127.0.0.1:${port}/`);
await page.fill("#host", `127.0.0.1:${port}`);
await page.fill("#pin", pin);
await page.click("#connect-btn");
await page.waitForSelector("#viewer-screen.active", { timeout: 15000 }).catch(() => fail("did not enter viewer"));
console.log("paired + streaming");

// Let a few frames land, then hard-kill the host mid-session.
await sleep(1500);
host.kill("SIGKILL");
console.log("host SIGKILLed (simulated network/host loss)");
await sleep(700); // viewer notices the close and begins retrying

hostLog += "\n--- host restarted ---\n";
host = startHost();
await sleep(500);

// The viewer must return to the active state on its own (token reconnect).
const recovered = await page
  .waitForSelector("#viewer-screen.active", { timeout: 20000 })
  .then(() => true)
  .catch(() => false);
if (!recovered) await fail("viewer did not auto-reconnect after host restart");

// And video must actually flow again: canvas pixels change.
const hashes = await page.evaluate(async () => {
  const canvas = document.getElementById("screen");
  const snap = () => {
    const c = document.createElement("canvas");
    c.width = 64; c.height = 64;
    c.getContext("2d").drawImage(canvas, 0, 0, 64, 64);
    const d = c.getContext("2d").getImageData(0, 0, 64, 64).data;
    let h = 0;
    for (let i = 0; i < d.length; i += 16) h = (h * 31 + d[i]) >>> 0;
    return h;
  };
  const a = snap();
  await new Promise((r) => setTimeout(r, 800));
  return [a, snap()];
});
if (hashes[0] === hashes[1]) await fail(`canvas frozen after reconnect (hash ${hashes[0]})`);
console.log(`streaming after reconnect: pixels changing (${hashes[0]} → ${hashes[1]})`);

await browser.close();
host.kill();
rmSync(dataDir, { recursive: true, force: true });
console.log("\nPASS: viewer auto-reconnected via token after host loss, video resumed");
