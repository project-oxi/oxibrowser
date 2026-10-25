#!/usr/bin/env bash
# Obtain the pinned Khronos WebGL conformance suite used by the opt-in runner.
#
# Usage: scripts/setup-webgl-conformance.sh
#        WEBGL_CONFORMANCE_REPO=/path/to/WebGL scripts/setup-webgl-conformance.sh
#
# Output: <repo>/sdk/tests. No suite files are copied into this repository.
set -euo pipefail

REV="3a65272e40f97c82de335825ea77c4da8e77a637"
REPO="${WEBGL_CONFORMANCE_REPO:-$HOME/khronos/WebGL}"

if [ ! -d "$REPO/.git" ]; then
  mkdir -p "$(dirname "$REPO")"
  git clone --no-checkout https://github.com/KhronosGroup/WebGL.git "$REPO"
fi
if [ -n "$(git -C "$REPO" status --porcelain)" ]; then
  echo "Khronos WebGL checkout has local changes; refusing to change its revision: $REPO" >&2
  exit 1
fi

git -C "$REPO" fetch --depth=1 origin "$REV"
git -C "$REPO" checkout --detach "$REV"
ACTUAL="$(git -C "$REPO" rev-parse HEAD)"
[ "$ACTUAL" = "$REV" ] || { echo "expected Khronos WebGL $REV, got $ACTUAL" >&2; exit 1; }
[ -f "$REPO/sdk/tests/js/js-test-pre.js" ] || { echo "missing Khronos test harness under $REPO/sdk/tests" >&2; exit 1; }
echo "Khronos WebGL revision: $ACTUAL"
echo "Test root: $REPO/sdk/tests"
echo "Run: WEBGL_CONFORMANCE_REPO='$REPO' scripts/run-webgl-conformance.sh"
