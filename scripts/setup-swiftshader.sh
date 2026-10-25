#!/usr/bin/env bash
# Build the pinned SwiftShader Vulkan ICD used by ANGLE's Vulkan backend.
#
# Usage: scripts/setup-swiftshader.sh
#        SWIFTSHADER_SRC=/path/to/swiftshader SWIFTSHADER_BUILD=/path/to/build scripts/setup-swiftshader.sh
#
# Output: $SWIFTSHADER_BUILD/Darwin/vk_swiftshader_icd.json and
#         $SWIFTSHADER_BUILD/Darwin/libvk_swiftshader.dylib (macOS arm64).
# Run ANGLE with:
#   OXI_ANGLE_PLATFORM=vulkan VK_ICD_FILENAMES=<manifest> cargo run \
#     -p oxibrowser-render --features webgl --example angle_smoke
set -euo pipefail

REV="cddb5c91b23ff70a1f607ea31610aa0d7f4b5ad5"
SRC="${SWIFTSHADER_SRC:-$HOME/sw-build/swiftshader}"
BUILD="${SWIFTSHADER_BUILD:-$SRC/build}"
MANIFEST="$BUILD/Darwin/vk_swiftshader_icd.json"
LIBRARY="$BUILD/Darwin/libvk_swiftshader.dylib"

if [ ! -d "$SRC/.git" ]; then
  mkdir -p "$(dirname "$SRC")"
  git clone --no-checkout https://github.com/google/swiftshader.git "$SRC"
fi
if [ -n "$(git -C "$SRC" status --porcelain)" ]; then
  echo "SwiftShader checkout has local changes; refusing to change its revision: $SRC" >&2
  exit 1
fi

git -C "$SRC" fetch --depth=1 origin "$REV"
git -C "$SRC" checkout --detach "$REV"
cmake -S "$SRC" -B "$BUILD" -DCMAKE_BUILD_TYPE=Release
cmake --build "$BUILD" --target vk_swiftshader --parallel

[ -f "$LIBRARY" ] || { echo "missing SwiftShader ICD library: $LIBRARY" >&2; exit 1; }
[ -f "$MANIFEST" ] || { echo "missing generated ICD manifest: $MANIFEST" >&2; exit 1; }
echo "SwiftShader revision: $(git -C "$SRC" rev-parse HEAD)"
echo "SwiftShader ICD manifest: $MANIFEST"
echo "Run: OXI_ANGLE_PLATFORM=vulkan VK_ICD_FILENAMES='$MANIFEST' cargo run -p oxibrowser-render --features webgl --example angle_smoke"
