#!/usr/bin/env node
// Unit tests for the letterbox coordinate mapping (src/input.ts mapToContent).
//
// The canvas element fills the viewport with `object-fit: contain`, so the
// video content is centered inside black bars whenever aspect ratios differ.
// A tap must be normalized against the *content box*, not the element box —
// getting this wrong was v0.2's "taps land on the wrong pixel" bug.
//
// Usage: node tests/mapping.mjs

import { execSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const webRoot = join(here, "..");

execSync(
  `npx esbuild src/input.ts --bundle --format=esm --outfile=/tmp/ndsp-input-bundle.mjs --log-level=error`,
  { cwd: webRoot, stdio: "inherit" },
);
const { mapToContent } = await import("/tmp/ndsp-input-bundle.mjs");

let failures = 0;
function eq(actual, expected, eps, label) {
  if (Math.abs(actual - expected) > eps) {
    console.error(`FAIL ${label}: got ${actual}, expected ${expected}`);
    failures++;
  } else {
    console.log(`ok   ${label}`);
  }
}

// 1. Same aspect ratio: element 1280x720, video 1920x1080 → plain scaling.
{
  const r = { left: 0, top: 0, width: 1280, height: 720 };
  const p = mapToContent(r, 1920, 1080, 640, 360);
  eq(p.x, 0.5, 1e-6, "same-aspect center x");
  eq(p.y, 0.5, 1e-6, "same-aspect center y");
}

// 2. Pillarboxing: portrait phone (390x844) showing 16:9 video.
//    Content box: w=390, h=390*9/16=219.375, offset y=(844-219.375)/2=312.3125.
{
  const r = { left: 0, top: 0, width: 390, height: 844 };
  // Tap exactly at the content's top-left corner.
  let p = mapToContent(r, 1920, 1080, 0, 312.3125);
  eq(p.x, 0, 1e-6, "pillarbox content top-left x");
  eq(p.y, 0, 1e-6, "pillarbox content top-left y");
  // Tap at the content's bottom-right corner.
  p = mapToContent(r, 1920, 1080, 390, 312.3125 + 219.375);
  eq(p.x, 1, 1e-6, "pillarbox content bottom-right x");
  eq(p.y, 1, 1e-6, "pillarbox content bottom-right y");
  // Tap in the black bar above the video clamps to y=0 (not a wrong pixel).
  p = mapToContent(r, 1920, 1080, 195, 100);
  eq(p.x, 0.5, 1e-6, "black-bar tap x");
  eq(p.y, 0, 1e-6, "black-bar tap clamps to content edge");
}

// 3. Letterboxing: ultrawide element (2560x800) showing 16:9 video.
//    Content box: h=800, w=800*16/9≈1422.22, offset x=(2560-1422.22)/2≈568.89.
{
  const r = { left: 0, top: 0, width: 2560, height: 800 };
  const contentW = (800 * 16) / 9;
  const offX = (2560 - contentW) / 2;
  let p = mapToContent(r, 1920, 1080, offX, 0);
  eq(p.x, 0, 1e-6, "letterbox content left edge");
  p = mapToContent(r, 1920, 1080, offX + contentW / 4, 400);
  eq(p.x, 0.25, 1e-6, "letterbox quarter x");
  eq(p.y, 0.5, 1e-6, "letterbox center y");
}

// 4. Element offset in the page (toolbar above the canvas).
{
  const r = { left: 10, top: 50, width: 1600, height: 900 };
  const p = mapToContent(r, 1920, 1080, 10 + 800, 50 + 450);
  eq(p.x, 0.5, 1e-6, "offset element center x");
  eq(p.y, 0.5, 1e-6, "offset element center y");
}

// 5. Degenerate content size (pre-first-frame): falls back to element box.
{
  const r = { left: 0, top: 0, width: 800, height: 600 };
  const p = mapToContent(r, 0, 0, 400, 300);
  eq(p.x, 0.5, 1e-6, "no-content fallback x");
  eq(p.y, 0.5, 1e-6, "no-content fallback y");
}

if (failures > 0) {
  console.error(`\n${failures} mapping test(s) FAILED`);
  process.exit(1);
}
console.log("\nall mapping tests passed");
