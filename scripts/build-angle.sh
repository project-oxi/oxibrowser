#!/usr/bin/env bash
# Build ANGLE (Metal + Vulkan backends) from the pinned chromium/8118 checkout
# and install the static libs into third_party/angle/<triple>.
# Source build enables the Vulkan backend for SwiftShader ICD verification;
# godot-angle-static prebuilts remain a Metal-only fallback.
#
# Prereq: ANGLE checkout at $ANGLE_SRC (default ~/angle-build/angle) produced by
#   git clone depot_tools; fetch --nohooks angle; gclient sync --force
#
# Usage: scripts/build-angle.sh
set -euo pipefail

ANGLE_SRC="${ANGLE_SRC:-$HOME/angle-build/angle}"
ANGLE_SRC="$(cd "$ANGLE_SRC" && pwd)"
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
angle_enable_vulkan = true
angle_enable_gl = false
angle_enable_d3d11 = false
angle_enable_null = false
angle_enable_egl = true
angle_enable_opengl = false
angle_enable_metal = true
angle_enable_wgpu = false
EOF

gn gen out/Release
# A Siso-generated output must be cleaned once before Ninja uses it. Preserve
# Ninja objects on repeat builds; GN/Ninja rebuilds targets when args change.
if [ ! -f out/Release/.ninja_log ]; then
  gn clean out/Release 2>/dev/null || true
fi
gn gen out/Release
ninja -C out/Release libEGL.dylib libGLESv2.dylib obj/libEGL_static.a obj/libGLESv2_static.a

mkdir -p "$OUT"
# Build ONE fat archive with unique member names. Composition (verified by
# the WebGL smoke + conformance runners):
#   * ANGLE EGL/GLES/Metal/Vulkan objects, excluding the dylib-flavor
#     `obj/libEGL/*` + `obj/libEGL_egl_loader/*` (those carry
#     ANGLE_USE_EGL_LOADER and dlopen libGLESv2 at runtime — static entry points
#     are used instead), plus the Vulkan Memory Allocator object and Vulkan
#     loader archive members required by the Vulkan backend;
#   * excluding the wgpu backend (drags in dawn/tint);
#   * including abseil-cpp, zlib and astc-encoder (ANGLE links against them).
# Member names are flattened to path-derived unique names: rustc repackages
# native archives into rlibs by basename and same-named objects would collide.
STAGE="$(mktemp -d)/stage"
mkdir -p "$STAGE"
cd out/Release
find obj -name "*.o" \
  \( -not -path "*third_party*" -o -path "*third_party/abseil-cpp*" \
     -o -path "*third_party/zlib*" -o -path "*astc*" \
     -o -path "*third_party/vulkan_memory_allocator*" \) \
  -not -path "*wgpu*" -not -path "obj/libEGL/*" -not -path "obj/libEGL_egl_loader/*" |
while read -r rel; do
  flat="obj_$(printf '%s' "$rel" | sed 's|^obj/||; s|/|__|g')"
  cp "$rel" "$STAGE/$flat"
done

# ANGLE's Vulkan loader is a thin static archive; copy its referenced objects
# into the staging area before flattening the final archive.
VULKAN_LOADER="obj/third_party/vulkan-loader/src/libvulkan.a"
if [ -f "$VULKAN_LOADER" ]; then
  LLVM_AR="$ANGLE_SRC/third_party/llvm-build/Release+Asserts/bin/llvm-ar"
  "$LLVM_AR" t "$ANGLE_SRC/out/Release/$VULKAN_LOADER" |
    while read -r member; do
      cp "$member" "$STAGE/obj_vulkan_loader__$(basename "$member")"
    done
fi
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
