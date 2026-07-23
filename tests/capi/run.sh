#!/usr/bin/env bash
# Build the C ABI as a cdylib from the merged crate, compile the C consumer test
# against it, and run it. Usage: tests/capi/run.sh
set -euo pipefail
cd "$(dirname "$0")/../.."

echo "building the libsqlite3-compatible cdylib (capi feature)..."
cargo rustc --release --lib --features capi --crate-type cdylib

LIB=target/release
CC=${CC:-cc}

echo "compiling + linking ctest.c against libgraphitesql..."
"$CC" -Wall -Wextra -Iinclude tests/capi/ctest.c -L"$LIB" -lgraphitesql \
  -o target/release/ctest

echo "running ctest..."
LD_LIBRARY_PATH="$LIB" target/release/ctest
