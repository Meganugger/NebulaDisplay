#!/usr/bin/env node
// Cross-stack SPAKE2 vectors: the TypeScript implementation (src/pake.ts)
// must be byte-identical to the Rust one (shared/protocol/src/pake.rs).
// The expected values below are asserted by the Rust unit test
// `pake::tests::fixed_vector_matches_reference` — if either side drifts,
// exactly one of the two tests fails.
//
// Usage: node tests/pake-vectors.mjs

import { execSync } from "node:child_process";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const webRoot = join(here, "..");

execSync(
  `npx esbuild src/pake.ts --bundle --format=esm --outfile=/tmp/ndsp-pake-vec.mjs --log-level=error`,
  { cwd: webRoot, stdio: "inherit" },
);
const { startPake } = await import("/tmp/ndsp-pake-vec.mjs");

const hex = (b) => [...b].map((x) => x.toString(16).padStart(2, "0")).join("");
const unhex = (s) => Uint8Array.from(s.match(/../g).map((h) => parseInt(h, 16)));

function assertEq(actual, expected, what) {
  if (actual !== expected) {
    console.error(`FAIL: ${what}\n  actual   ${actual}\n  expected ${expected}`);
    process.exit(1);
  }
  console.log(`ok: ${what}`);
}

// ---- fixed vector (mirrors the Rust test exactly) --------------------------
const salt = Uint8Array.from({ length: 16 }, (_, i) => i);
const nonce = Uint8Array.from({ length: 16 }, (_, i) => 16 + i);
const client = startPake("424242", salt, nonce, 0x1111222233334444n);

assertEq(
  hex(client.share),
  "046ced788260bc0c17179d3458786ae6470cff0f3306edb09889b95efc763dec92" +
    "4a7c73a4bc173da1a1bf7ebdfbdb860094070d32305ace2fc4b68bf613c17b29",
  "client share matches Rust vector",
);

const serverShare = unhex(
  "0437ea8ba904de147c0b3671d2d04abd97814a7926023bcaa0f1ea7228806f64be" +
    "4da435353f82f19b01f23767199ac68f482904001e38abb88443ef3fd4ad2800",
);
const key = client.finish(
  serverShare,
  new Uint8Array(65).fill(0xaa),
  new Uint8Array(65).fill(0xbb),
);
assertEq(
  hex(key),
  "4a0229ce2ed537da978bc78db844b445d0dc94262a01d7c8b404b74bb1a516c1",
  "pair key matches Rust vector",
);

// ---- behavioral checks ------------------------------------------------------
// Wrong-PIN shares are still valid curve points but yield a different key.
const wrong = startPake("424243", salt, nonce, 0x1111222233334444n);
const wrongKey = wrong.finish(
  serverShare,
  new Uint8Array(65).fill(0xaa),
  new Uint8Array(65).fill(0xbb),
);
if (hex(wrongKey) === hex(key)) {
  console.error("FAIL: wrong PIN produced the same key");
  process.exit(1);
}
console.log("ok: wrong PIN diverges");

// Fresh runs are randomized (nothing PIN-dependent is visible on the wire).
const a = startPake("424242", salt, nonce);
const b = startPake("424242", salt, nonce);
if (hex(a.share) === hex(b.share)) {
  console.error("FAIL: shares are deterministic across runs");
  process.exit(1);
}
console.log("ok: shares are randomized");

// Garbage server shares are rejected.
let threw = false;
try {
  startPake("424242", salt, nonce).finish(
    new Uint8Array(65).fill(0xff),
    new Uint8Array(65),
    new Uint8Array(65),
  );
} catch {
  threw = true;
}
if (!threw) {
  console.error("FAIL: invalid server share accepted");
  process.exit(1);
}
console.log("ok: invalid share rejected");

console.log("\nPAKE vectors PASS");
