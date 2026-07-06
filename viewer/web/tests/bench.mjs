#!/usr/bin/env node
// Reproducible pipeline benchmark: real nebulad host (test-pattern source,
// worst-case full-frame motion) + real Chromium viewer, measured through the
// same instrumentation the product ships (nothing estimated).
//
// For every resolution × profile combination it reports, after a warmup:
//   fps      — frames decoded+presented per second (viewer-measured)
//   e2e      — capture-timestamp → presentation, ms (synced clocks)
//   net+host — capture-timestamp → envelope arrival, ms
//   enc      — host encode time/frame, ms (cvt = color-convert share)
//   age      — capture → encode-start scheduling wait, ms
//   send     — seal+socket write, ms
//   dec      — viewer decode time/frame, ms
//   Mbps     — actual bitrate on the wire
//
// Usage: node tests/bench.mjs [--quick] [--json out.json]
// Notes: results are loopback (no physical network); the test pattern's
// full-frame motion is *worst case* for the encoder — real desktops with
// dirty-region elision encode less. Run on the deployment hardware for
// meaningful absolute numbers; this harness makes runs comparable.

import { chromium } from "playwright";
import { spawn } from "node:child_process";
import { existsSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const webRoot = join(here, "..");
const repoRoot = join(webRoot, "..", "..");
const quick = process.argv.includes("--quick");
const jsonOut = process.argv.includes("--json")
  ? process.argv[process.argv.indexOf("--json") + 1]
  : null;

const BIN =
  process.env.NEBULAD_BIN ??
  (existsSync(join(repoRoot, "target", "release", "nebulad"))
    ? join(repoRoot, "target", "release", "nebulad")
    : join(repoRoot, "target", "debug", "nebulad"));

const MATRIX = quick
  ? [{ size: "1280x720", profile: "video" }]
  : [
      { size: "1280x720", profile: "office" },
      { size: "1280x720", profile: "video" },
      { size: "1920x1080", profile: "video" },
      { size: "1920x1080", profile: "gaming" },
      { size: "2560x1440", profile: "video" },
      { size: "3840x2160", profile: "video" },
    ];

const WARMUP_MS = quick ? 3000 : 6000;
const MEASURE_MS = quick ? 4000 : 10000;
const PORT = 41987;
const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

async function benchOne(browser, size, profile) {
  const dataDir = mkdtempSync(join(tmpdir(), "ndsp-bench-"));
  const host = spawn(
    BIN,
    [
      "--test-pattern", "--bind", "127.0.0.1",
      "--port", String(PORT), "--panel-port", String(PORT - 1),
      "--discovery-port", "0", "--data-dir", dataDir,
      "--capture-size", size, "--web-dir", join(webRoot, "dist"),
    ],
    { stdio: ["ignore", "pipe", "pipe"] },
  );
  let pin = null;
  let log = "";
  host.stdout.on("data", (d) => {
    log += d.toString();
    const m = log.match(/PIN \(single-use\)[^\n]*\n\s+(\d{4,10})/);
    if (m) pin = m[1];
  });
  host.stderr.on("data", (d) => (log += d.toString()));
  for (let i = 0; i < 100 && pin === null; i++) await sleep(100);
  if (!pin) throw new Error(`host did not print a PIN\n${log.slice(-500)}`);

  const page = await browser.newPage();
  try {
    await page.goto(`http://127.0.0.1:${PORT}/`);
    await page.fill("#pin", pin);
    await page.click("#connect-btn");
    await page.waitForSelector("#viewer-screen.active", { timeout: 15000 });
    await page.selectOption("#profile", profile);
    await page.click("#stats-btn");
    await sleep(WARMUP_MS);

    // Sample the overlay repeatedly; average the numeric fields.
    const samples = [];
    const t0 = Date.now();
    while (Date.now() - t0 < MEASURE_MS) {
      await sleep(1000);
      const text = await page.textContent("#stats-overlay");
      const g = (re) => {
        const m = text.match(re);
        return m ? parseFloat(m[1]) : NaN;
      };
      samples.push({
        fps: g(/decode fps (\d+(?:\.\d+)?)/),
        dec: g(/decode avg (\d+(?:\.\d+)?)/),
        e2e: g(/e2e\s+(\d+(?:\.\d+)?)/),
        net: g(/net\+host\s+(\d+(?:\.\d+)?)/),
        present: g(/present\s+(\d+(?:\.\d+)?)/),
        enc: g(/enc (\d+(?:\.\d+)?) ms/),
        cvt: g(/cvt (\d+(?:\.\d+)?)/),
        age: g(/age (\d+(?:\.\d+)?)/),
        send: g(/send (\d+(?:\.\d+)?)/),
        mbps: g(/(\d+(?:\.\d+)?) Mbps/),
      });
    }
    const avg = (k) => {
      const v = samples.map((s) => s[k]).filter((x) => !Number.isNaN(x));
      return v.length ? v.reduce((a, b) => a + b, 0) / v.length : NaN;
    };
    return {
      size, profile,
      fps: avg("fps"), e2e: avg("e2e"), net: avg("net"), present: avg("present"),
      enc: avg("enc"), cvt: avg("cvt"), age: avg("age"), send: avg("send"),
      dec: avg("dec"), mbps: avg("mbps"),
    };
  } finally {
    await page.close();
    host.kill();
    rmSync(dataDir, { recursive: true, force: true });
    await sleep(300);
  }
}

const browser = await chromium.launch();
const results = [];
for (const { size, profile } of MATRIX) {
  process.stderr.write(`bench ${size} ${profile}…\n`);
  try {
    results.push(await benchOne(browser, size, profile));
  } catch (e) {
    process.stderr.write(`  FAILED: ${e.message}\n`);
    results.push({ size, profile, error: String(e.message) });
  }
}
await browser.close();

const f = (x, d = 1) => (Number.isNaN(x) || x === undefined ? "—" : x.toFixed(d));
console.log("| size | profile | fps | e2e ms | net+host ms | present ms | enc ms | cvt ms | age ms | send ms | dec ms | Mbps |");
console.log("|---|---|---|---|---|---|---|---|---|---|---|---|");
for (const r of results) {
  if (r.error) {
    console.log(`| ${r.size} | ${r.profile} | ERROR: ${r.error} |`);
    continue;
  }
  console.log(
    `| ${r.size} | ${r.profile} | ${f(r.fps)} | ${f(r.e2e)} | ${f(r.net)} | ${f(r.present, 2)} | ${f(r.enc)} | ${f(r.cvt)} | ${f(r.age)} | ${f(r.send)} | ${f(r.dec)} | ${f(r.mbps)} |`,
  );
}
if (jsonOut) writeFileSync(jsonOut, JSON.stringify(results, null, 2));
