/**
 * Browser smoke test: boots the real nebula-host binary (test-pattern
 * source, TLS off for the harness), then drives the real web viewer in
 * headless Chromium: pair with PIN → stream → assert frames render on the
 * canvas → screenshot artifacts.
 *
 * Run: node tests/browser-smoke.mjs   (from repo root, after cargo build
 * and viewer/web npm run build; see scripts/smoke.sh)
 */

import { spawn } from "node:child_process";
import { mkdirSync } from "node:fs";
import { createRequire } from "node:module";

// Playwright is installed as a devDependency of viewer/web; resolve it from
// there so the repo root stays dependency-free.
const require = createRequire(new URL("../viewer/web/package.json", import.meta.url));
const { chromium } = require("playwright");

const PORT = 39555;
const HOST_BIN = process.env.HOST_BIN ?? "target/debug/nebula-host";
const ARTIFACTS = "tests/artifacts";

function log(...args) {
  console.log("[smoke]", ...args);
}

async function waitFor(fn, timeoutMs, what) {
  const start = Date.now();
  for (;;) {
    try {
      const v = await fn();
      if (v) return v;
    } catch {
      /* retry */
    }
    if (Date.now() - start > timeoutMs) throw new Error(`timeout waiting for ${what}`);
    await new Promise((r) => setTimeout(r, 250));
  }
}

mkdirSync(ARTIFACTS, { recursive: true });

log("starting host:", HOST_BIN);
const host = spawn(
  HOST_BIN,
  [
    "--port", String(PORT),
    "--no-tls",
    "--source", "test",
    "--web-dir", "viewer/web/dist",
    "--config", `/tmp/nebula-smoke-${process.pid}/host.toml`,
  ],
  { stdio: ["ignore", "inherit", "inherit"] },
);
const stopHost = () => { try { host.kill(); } catch { /* gone */ } };
process.on("exit", stopHost);

let failed = false;
try {
  await waitFor(
    () => fetch(`http://127.0.0.1:${PORT}/api/info`).then((r) => r.ok),
    15000,
    "host to come up",
  );
  log("host is up");

  // Issue a pairing PIN through the loopback admin API (same thing the
  // control panel button does).
  const pinRes = await fetch(`http://127.0.0.1:${PORT}/api/admin/pin`, { method: "POST" });
  const { pin } = await pinRes.json();
  log("pairing PIN issued:", pin);

  const browser = await chromium.launch();
  const page = await browser.newPage({ viewport: { width: 1280, height: 800 } });
  page.on("console", (m) => m.type() === "error" && console.error("[page]", m.text()));
  page.on("pageerror", (e) => { console.error("[pageerror]", e); failed = true; });

  // --- Control panel loads and shows status ---
  await page.goto(`http://127.0.0.1:${PORT}/`);
  await page.waitForSelector("#status-kv dd", { timeout: 10000 });
  await page.screenshot({ path: `${ARTIFACTS}/panel.png` });
  log("control panel renders ✔");

  // --- Viewer: pair + stream ---
  await page.goto(`http://127.0.0.1:${PORT}/view/`);
  await page.waitForSelector("#pin-form:not([hidden])", { timeout: 15000 });
  log("viewer prompts for pairing ✔");
  await page.fill("#pin-input", pin);
  await page.click("#pin-btn");

  await page.waitForSelector("#toolbar:not([hidden])", { timeout: 15000 });
  log("session started ✔");

  // Frames actually render: the canvas must be non-black and *changing*.
  const sample = () =>
    page.evaluate(() => {
      const c = document.getElementById("screen");
      const ctx = c.getContext("2d");
      const d = ctx.getImageData(0, 0, c.width, c.height).data;
      let sum = 0;
      for (let i = 0; i < d.length; i += 4097) sum += d[i];
      return sum;
    });
  await waitFor(async () => (await sample()) > 0, 10000, "non-black canvas");
  const s1 = await sample();
  await new Promise((r) => setTimeout(r, 1500));
  const s2 = await sample();
  if (s1 === s2) throw new Error("canvas is not animating (identical samples)");
  log(`canvas is live and animating ✔ (sample ${s1} → ${s2})`);

  // Stats overlay shows real numbers.
  await page.click("#stats-btn");
  await waitFor(
    async () => (await page.textContent("#stats-overlay"))?.includes("fps"),
    8000,
    "stats overlay",
  );
  log("stats overlay ✔");
  await page.screenshot({ path: `${ARTIFACTS}/viewer-streaming.png` });

  // Reconnect path: token is stored, reload should stream without a PIN.
  await page.reload();
  await page.waitForSelector("#toolbar:not([hidden])", { timeout: 15000 });
  log("token reconnect without PIN ✔");

  await browser.close();
  log("ALL SMOKE CHECKS PASSED");
} catch (e) {
  failed = true;
  console.error("[smoke] FAILED:", e);
} finally {
  stopHost();
}
process.exit(failed ? 1 : 0);
