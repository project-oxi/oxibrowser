#!/usr/bin/env bash
# Build and run the opt-in Khronos conformance subset against OxiBrowser.
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REPO="${WEBGL_CONFORMANCE_REPO:-$HOME/khronos/WebGL}"
WEBGL_CONFORMANCE_REPO="$REPO" "$ROOT/scripts/setup-webgl-conformance.sh"
export WEBGL_CONFORMANCE_REPO="$REPO"
cargo test -p oxibrowser-core --features webgl --test webgl_conformance -- --ignored --nocapture
