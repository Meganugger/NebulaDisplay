#!/usr/bin/env bash
# Full local verification: everything CI runs, in order.
set -euo pipefail
cd "$(dirname "$0")/.."
echo "== rustfmt ==";  cargo fmt --all --check
echo "== clippy ==";   cargo clippy --workspace --all-targets -- -D warnings
echo "== tests ==";    cargo test --workspace
echo "== win check =="; cargo check --workspace --target x86_64-pc-windows-msvc --no-default-features
echo "== web ==";      (cd viewer/web && npm run build)
echo "== browser =="; node tests/browser-smoke.mjs
echo "ALL GREEN"
