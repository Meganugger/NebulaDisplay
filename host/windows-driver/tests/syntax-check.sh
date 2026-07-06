#!/usr/bin/env bash
# Driver syntax/type check against stub WDK headers (see stub/README note in
# each header). This is NOT a WDK build — it catches C++ errors, API-shape
# mismatches (structs modeled from Microsoft's public documentation) and
# version-gate breakage on every commit, on any platform. Real compile +
# InfVerif still require a Windows machine with the WDK (docs/DRIVER.md).
set -euo pipefail
cd "$(dirname "$0")"
CXX="${CXX:-clang++}"
for MINOR in 10 4; do
  echo "syntax-check: IDDCX_VERSION_MINOR=$MINOR"
  "$CXX" -std=c++17 -fsyntax-only -Wall -Wextra \
    -Wno-unused-parameter -Wno-null-dereference \
    -DIDDCX_VERSION_MAJOR=1 -DIDDCX_VERSION_MINOR=$MINOR \
    -DIDDCX_MINIMUM_VERSION_REQUIRED=4 \
    -I stub -I ../include ../src/Driver.cpp
done
echo "driver syntax check PASSED (both IddCx header models)"
