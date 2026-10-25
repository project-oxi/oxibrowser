#!/usr/bin/env bash
# Provision pinned ANGLE static libraries for the `webgl` cargo feature.
#
# Source: godotengine/godot-angle-static, release tag `chromium/8118`
# (ANGLE at Chromium 8118, Metal backend, built for macOS arm64 + x86_64).
# The archives contain libEGL / libGLES / libANGLE (static, release).
#
# Usage: scripts/setup-angle.sh            (installs for this machine's arch)
#        OXI_TARGET_ARCH=x86_64-apple-darwin scripts/setup-angle.sh
#
# Output: third_party/angle/<rust-target-triple>/{libEGL.a,libGLES.a,libANGLE.a}
# plus LICENSE.angle and VERSION. `third_party/angle` is gitignored; binaries
# are never committed. crates/oxibrowser-render/build.rs links from this layout
# and fails with a pointer to this script when the feature is on but files are
# missing.
set -euo pipefail

BASE="https://github.com/godotengine/godot-angle-static/releases/download/chromium/8118"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
DEST="$ROOT/third_party/angle"

ARCH="${OXI_TARGET_ARCH:-}"
if [ -z "$ARCH" ]; then
  case "$(uname -sm)" in
    Darwin\ arm64) ARCH="aarch64-apple-darwin" ;;
    Darwin\ x86_64) ARCH="x86_64-apple-darwin" ;;
    *) echo "unsupported host: $(uname -sm); set OXI_TARGET_ARCH=<rust-triple>" >&2; exit 1 ;;
  esac
fi

case "$ARCH" in
  aarch64-apple-darwin)
    ASSET="godot-angle-static-arm64-macos-release.zip"
    SHA256="b8161e6226d209036be639ae29ee87a888264e260af4a0f3174d605b6063b013"
    SUFFIX="macos.arm64"
    ;;
  x86_64-apple-darwin)
    ASSET="godot-angle-static-x86_64-macos-release.zip"
    SHA256="f981e2527358868970c5d236b1dfc6eaaac180796ce23f5e389425610be89aba"
    SUFFIX="macos.x86_64"
    ;;
  *) echo "no pinned artifact for $ARCH" >&2; exit 1 ;;
esac

OUT="$DEST/$ARCH"
if [ -f "$OUT/libANGLE_all.a" ]; then
  echo "angle: already provisioned at $OUT"
  exit 0
fi

mkdir -p "$OUT" "$DEST/.dl"
ZIP="$DEST/.dl/$ASSET"
curl -sSL --fail -o "$ZIP" "$BASE/$ASSET"
echo "$SHA256  $ZIP" | shasum -a 256 -c - >/dev/null
unzip -o -q "$ZIP" -d "$OUT"

for lib in EGL GLES ANGLE; do
  src="$OUT/lib${lib}.${SUFFIX}.a"
  [ -f "$src" ] || { echo "missing $src in archive" >&2; exit 1; }
  mv "$src" "$OUT/lib${lib}.a"
done
rm -f "$OUT"/*.a.tmp

curl -sSL --fail -o "$OUT/LICENSE.angle" \
  "https://raw.githubusercontent.com/google/angle/chromium%2F8118/LICENSE"
printf 'ANGLE chromium/8118 (godot-angle-static chromium/8118)\nMetal backend, static, release\n' > "$OUT/VERSION"

# Assemble the single flat archive build.rs links. The three upstream archives
# are fat; extract their members and flatten with unique (archive-prefixed)
# names — rustc repackages native archives into rlibs by basename and
# same-named members would collide.
STAGE="$(mktemp -d)/stage"
mkdir -p "$STAGE"
for lib in EGL GLES ANGLE; do
  D="$STAGE/_x_$lib"
  mkdir -p "$D"
  (cd "$D" && ar x "$OUT/lib${lib}.a")
  for f in "$D"/*.o; do
    [ -e "$f" ] || continue
    cp "$f" "$STAGE/${lib}__$(basename "$f")"
  done
done
rm -f "$OUT/libEGL.a" "$OUT/libGLES.a" "$OUT/libANGLE.a"
# shellcheck disable=SC2046
ar -rcs "$OUT/libANGLE_all.a" "$STAGE"/*.o
rm -rf "$(dirname "$STAGE")"

echo "angle: installed $ARCH -> $OUT (libANGLE_all.a)"
