#!/usr/bin/env bash
# Build the cdylib, compile the C test against it, and run it.
# Usage: tests/run.sh
set -euo pipefail
cd "$(dirname "$0")/.."

echo "building libgraphitesql_capi..."
cargo build --release

LIB=target/release
CC=${CC:-cc}

echo "compiling + linking ctest.c against the shim..."
"$CC" -Wall -Wextra -Iinclude tests/ctest.c -L"$LIB" -lgraphitesql_capi \
  -o target/release/ctest

echo "running ctest (shim)..."
LD_LIBRARY_PATH="$LIB" target/release/ctest
