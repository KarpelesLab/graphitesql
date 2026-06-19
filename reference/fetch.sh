#!/usr/bin/env bash
#
# Fetch the upstream SQLite source tree used as a *reference* while building
# graphitesql. Nothing here is compiled or linked into the crate; it exists
# only so contributors can read the canonical C implementation and the on-disk
# file-format documentation next to the Rust code that re-implements it.
#
# The downloads are hash-verified against the values published on
# https://www.sqlite.org/download.html and are intentionally git-ignored
# (see ../.gitignore) because they are large and fully reproducible.
#
# Usage:  ./reference/fetch.sh
#
set -euo pipefail

# Pin to the exact upstream release graphitesql currently targets.
SQLITE_VERSION="3.53.2"
SQLITE_YEAR="2026"
SRC_ZIP="sqlite-src-3530200.zip"
SRC_SHA3="490ec7af32a6bfa5f3e05dc279c04286cfe3f328def4a8b7344e3fa20be18a4c"

cd "$(dirname "$0")"

echo "Fetching SQLite ${SQLITE_VERSION} reference source ..."
curl -fL --proto '=https' -o "${SRC_ZIP}" \
  "https://www.sqlite.org/${SQLITE_YEAR}/${SRC_ZIP}"

echo "Verifying SHA3-256 ..."
got="$(python3 -c "import hashlib,sys;print(hashlib.sha3_256(open(sys.argv[1],'rb').read()).hexdigest())" "${SRC_ZIP}")"
if [ "${got}" != "${SRC_SHA3}" ]; then
  echo "ERROR: hash mismatch for ${SRC_ZIP}" >&2
  echo "  expected ${SRC_SHA3}" >&2
  echo "  got      ${got}" >&2
  exit 1
fi
echo "  ok: ${got}"

echo "Extracting ..."
unzip -o -q "${SRC_ZIP}"

# Also grab the human-readable on-disk file-format spec, our compatibility bible.
echo "Fetching file-format documentation ..."
curl -fL --proto '=https' -o sqlite-fileformat2.html \
  "https://www.sqlite.org/fileformat2.html"

echo
echo "Done. Reference tree: reference/sqlite-src-${SRC_ZIP#sqlite-src-}"
echo "Key files to read:"
echo "  src/btree.c    -> graphitesql btree"
echo "  src/pager.c    -> graphitesql pager"
echo "  src/wal.c      -> graphitesql pager::wal"
echo "  src/vdbe*.c    -> graphitesql vdbe"
echo "  src/parse.y    -> graphitesql sql grammar"
echo "  src/tokenize.c -> graphitesql sql::token"
echo "  src/where*.c   -> graphitesql planner"
