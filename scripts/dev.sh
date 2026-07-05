#!/usr/bin/env bash
# One-shot dev loop: build web UI + run the host with the test source.
set -euo pipefail
cd "$(dirname "$0")/.."
(cd viewer/web && npm install --no-audit --no-fund && npm run build)
exec cargo run -p nebula-host -- --source test "$@"
