#!/usr/bin/env bash
# Build ANGLE (Metal backend) from the pinned chromium/8118 checkout and
# install the static libs into third_party/angle/<triple>, replacing the
# godot-angle-static prebuilts (whose macOS Metal backend initializes but does
# not rasterize draws — see docs/designs/2026-10-25-webgl-angle.md).
#
# Prereq: ANGLE checkout at $ANGLE_SRC (default ~/angle-build/angle) produced by
#   git clone depot_tools; fetch --nohooks angle; gclient sync --force
#
# Usage: scripts/build-angle.sh
set -euo pipefail

ANGLE_SRC="${ANGLE_SRC:-$HOME/angle-build/angle}"
REV="chromium/8118"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/third_party/angle/$(uname -m | sed 's/arm64/aarch64/;s/x86_64/x86_64/')-apple-darwin"

export PATH="$HOME/angle-build/depot_tools:$PATH"

cd "$ANGLE_SRC"
CURRENT="$(git rev-parse --abbrev-ref HEAD@{upstream} 2>/dev/null || true)"
git fetch origin "$REV" 2>/dev/null || true
git checkout -q "$REV" 2>/dev/null || git checkout -q "$(git rev-parse origin/${REV#*/})"

mkdir -p out/Release
cat > out/Release/args.gn <<'EOF'
is_debug = false
is_component_build = false
use_siso = false
# Link against the system libc++ (std::__1), not Chromium's custom-ABI
# libc++ (__Cr) which would require shipping the ANGLE toolchain runtime.
use_custom_libcxx = false
use_custom_libcxx_for_host = false
# System libc++ trips Chromium's unsafe-buffer lints in ANGLE sources.
treat_warnings_as_errors = false
# Drop the perfetto tracing dep (its objects live under third_party and are
# not part of our static closure).
angle_enable_perfetto = false
angle_build_all = false
angle_build_tests = false
angle_enable_vulkan = false
angle_enable_gl = false
angle_enable_d3d9 = false
angle_enable_d3d11 = false
angle_enable_null = false
angle_enable_egl = true
angle_enable_opengl = false
angle_enable_metal = true
angle_enable_wgpu = false
EOF

gn gen out/Release
# Switching build tools (siso → ninja) requires a clean out dir.
gn clean out/Release 2>/dev/null || true
gn gen out/Release
ninja -C out/Release libEGL.dylib libGLESv2.dylib obj/libEGL_static.a obj/libGLESv2_static.a

mkdir -p "$OUT"
# Build ONE fat archive with unique member names. Composition (verified by
# crates/oxibrowser-core/src/js/webgl/session_tests.rs):
#   * all non-third_party ANGLE objects, EXCLUDING the dylib-flavor
#     `obj/libEGL/*` + `obj/libEGL_egl_loader/*` (those carry
#     ANGLE_USE_EGL_LOADER and dlopen libGLESv2 at runtime — the static
#     `libEGL_static`/`libGLESv2_static` entry points are used instead),
#   * excluding the wgpu backend (drags in dawn/tint),
#   * including abseil-cpp, zlib and astc-encoder (ANGLE links against them).
# Member names are flattened to path-derived unique names: rustc repackages
# native archives into rlibs by basename and same-named objects would collide.
STAGE="$(mktemp -d)/stage"
mkdir -p "$STAGE"
cd out/Release
find obj -name "*.o" \
  \( -not -path "*third_party*" -o -path "*third_party/abseil-cpp*" \
     -o -path "*third_party/zlib*" -o -path "*astc*" \) \
  -not -path "*wgpu*" -not -path "obj/libEGL/*" -not -path "obj/libEGL_egl_loader/*" |
while read -r rel; do
  flat="obj_$(printf '%s' "$rel" | sed 's|^obj/||; s|/|__|g')"
  cp "$rel" "$STAGE/$flat"
done
COUNT=$(ls "$STAGE" | wc -l | tr -d ' ')
[ "$COUNT" -gt 300 ] || { echo "suspiciously few objects ($COUNT); ninja layout changed?" >&2; exit 1; }
# shellcheck disable=SC2046
ar -rcs "$OUT/libANGLE_all.a" "$STAGE"/obj_*.o
rm -rf "$(dirname "$STAGE")"
cd ../..
echo "libANGLE_all.a: $COUNT objects, $(du -h "$OUT/libANGLE_all.a" | cut -f1)"

git -C "$ANGLE_SRC" log -1 --format="%H %cd" > "$OUT/VERSION"
printf 'built from %s (scripts/build-angle.sh)\n' "$(git -C "$ANGLE_SRC" rev-parse HEAD)" >> "$OUT/VERSION"

echo "angle: installed source build -> $OUT"
