#!/usr/bin/env bash
# A/B measurement: how big is BoringSSL on armv7hf, static vs shared?
#
# Answers the C-plan question: if BoringSSL shipped as a .so next to meow
# instead of being statically linked in, how many bytes would that .so cost?
#
# SIZE-ONLY experiment. It does NOT produce a runnable meow that dynamically
# links the .so — boring-sys hardcodes
# `cargo:rustc-link-lib=static=crypto` / `static=ssl` (build/main.rs:732-733),
# so switching the real binary needs a boring-sys patch + a crate feature.
# Measure first; decide later.
#
# Runs inside the armv7 toolchain container (has cmake + arm gcc + strip):
#   docker run --rm -v "$PWD:/src" -w /src \
#     ghcr.io/rust-cross/rust-musl-cross:armv7-musleabihf-amd64 \
#     bash scripts/measure-boringssl-shared.sh
#
# Search order for BoringSSL source:
#   1. BSSL_SRC env var
#   2. cargo registry copy of boring-sys 5.2.0 (after `cargo fetch --locked`)
#
# Output: stripped byte sizes for both variants, written to stdout and to
# $GITHUB_STEP_SUMMARY when running in CI.

set -euo pipefail

TARGET=armv7-unknown-linux-musleabihf
CC="${TARGET}-gcc"
CXX="${TARGET}-g++"
AR="${TARGET}-ar"
STRIP="${TARGET}-strip"

for tool in cmake "$CC" "$CXX" "$AR" "$STRIP"; do
  command -v "$tool" >/dev/null 2>&1 || { echo "missing tool: $tool" >&2; exit 1; }
done

# --- Locate BoringSSL source -------------------------------------------
if [ -z "${BSSL_SRC:-}" ]; then
  BSSL_SRC=$(find "${CARGO_HOME:-$HOME/.cargo}/registry/src" \
    -type d -path '*/boring-sys-5.2.0/deps/boringssl' 2>/dev/null | head -1)
fi
[ -n "${BSSL_SRC:-}" ] && [ -f "$BSSL_SRC/CMakeLists.txt" ] || {
  echo "BoringSSL source not found; run cargo fetch --locked first" >&2; exit 1;
}
echo "BoringSSL source: $BSSL_SRC"

WORK_ROOT="${WORK_ROOT:-/tmp/bssl-ab}"
rm -rf "$WORK_ROOT"; mkdir -p "$WORK_ROOT"

# Self-contained toolchain file. Do NOT use ${BSSL_SRC}/../cmake/armv7-linux.cmake:
# CI binds only the boringssl/ subtree, so the sibling cmake/ dir is absent.
cat > "$WORK_ROOT/toolchain.cmake" <<'EOF'
set(CMAKE_SYSTEM_NAME Linux)
set(CMAKE_SYSTEM_PROCESSOR arm)
set(CMAKE_CROSSCOMPILING TRUE)
EOF

# Flags mirror the app's release profile as closely as a standalone C build
# can: -Os (size), section GC, no unwind tables. No LTO — Rust does fat LTO
# at final link; this measures the library contribution, not the meow delta.
COMMON_FLAGS="-Os -ffunction-sections -fdata-sections -fno-unwind-tables -fno-asynchronous-unwind-tables"

build_one() {
  local mode="$1" shared="OFF" bdir="$WORK_ROOT/$1"
  [ "$mode" = shared ] && shared="ON"
  echo "=== configure $mode ==="
  cmake -S "$BSSL_SRC" -B "$bdir" -G Ninja \
    -DCMAKE_C_COMPILER="$CC" -DCMAKE_CXX_COMPILER="$CXX" \
    -DCMAKE_ASM_COMPILER="$CC" -DCMAKE_AR="$AR" \
    -DCMAKE_TOOLCHAIN_FILE="$WORK_ROOT/toolchain.cmake" \
    -DCMAKE_SYSTEM_PROCESSOR=armv7 \
    -DCMAKE_TRY_COMPILE_TARGET_TYPE=STATIC_LIBRARY \
    -DCMAKE_BUILD_TYPE=Release \
    -DBUILD_SHARED_LIBS="$shared" \
    -DCMAKE_C_FLAGS="$COMMON_FLAGS" -DCMAKE_CXX_FLAGS="$COMMON_FLAGS" \
    >"$WORK_ROOT/$mode-config.log" 2>&1 || { dump_failure "$mode-config"; return 1; }
  echo "=== build $mode ==="
  cmake --build "$bdir" --target ssl crypto -j 4 \
    >"$WORK_ROOT/$mode-build.log" 2>&1 || { dump_failure "$mode-build"; return 1; }
}

# Failure logs go to the step summary too — the raw job log is not readable
# without GitHub auth, but the summary is always visible on the run page.
dump_failure() {
  local tag="$1"
  echo "FAILED: $tag (last 60 lines)" >&2
  tail -60 "$WORK_ROOT/$tag.log" >&2
  echo "::error::A/B $tag failed — see step summary for the last 60 log lines"
  if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    { echo "## A/B build failed: $tag"; echo '```'; tail -60 "$WORK_ROOT/$tag.log"; echo '```'; } \
      >> "$GITHUB_STEP_SUMMARY" 2>/dev/null || true
  fi
}

build_one static
build_one shared

# --- Strip + measure ----------------------------------------------------
size_of() { stat -c '%s' "$1" 2>/dev/null || wc -c < "$1"; }
# Copy first (dereference symlinks), then strip in place. binutils strip has
# no portable -o; stripping a copy avoids touching the build tree.
strip_copy() {
  cp -L "$1" "$2" || return 1
  "$STRIP" "$2" || return 1
}

echo "=== produced artifacts ==="
find "$WORK_ROOT/static" "$WORK_ROOT/shared" \
  \( -name 'libssl.a' -o -name 'libcrypto.a' -o -name 'libssl.so*' -o -name 'libcrypto.so*' \) \
  -printf '%y %p -> %l\n' 2>/dev/null | sort

# Prefer the unversioned .so; fall back to the real versioned file, and
# dereference symlinks so cp gets bytes rather than a dangling link.
pick_so() {
  local dir="$1" base="$2"
  local f
  f=$(find "$dir" -maxdepth 2 -type f -name "lib${base}.so" | head -1)
  [ -z "$f" ] && f=$(find "$dir" -maxdepth 2 -type f -name "lib${base}.so.*" | head -1)
  [ -z "$f" ] && f=$(find "$dir" -maxdepth 2 -type l -name "lib${base}.so*" | head -1)
  printf '%s' "$f"
}

S_SSL=$(find "$WORK_ROOT/static" -name 'libssl.a' | head -1)
S_CRYPTO=$(find "$WORK_ROOT/static" -name 'libcrypto.a' | head -1)
H_SSL=$(pick_so "$WORK_ROOT/shared" ssl)
H_CRYPTO=$(pick_so "$WORK_ROOT/shared" crypto)

for v in S_SSL:$S_SSL S_CRYPTO:$S_CRYPTO H_SSL:$H_SSL H_CRYPTO:$H_CRYPTO; do
  [ -n "${v#*:}" ] || { echo "missing artifact: ${v%%:*}" >&2; exit 1; }
done

strip_copy "$S_SSL"    "$WORK_ROOT/static/ssl.strip.a"
strip_copy "$S_CRYPTO" "$WORK_ROOT/static/crypto.strip.a"
strip_copy "$H_SSL"    "$WORK_ROOT/shared/ssl.strip.so"
strip_copy "$H_CRYPTO" "$WORK_ROOT/shared/crypto.strip.so"

s_ssl=$(size_of "$WORK_ROOT/static/ssl.strip.a")
s_crypto=$(size_of "$WORK_ROOT/static/crypto.strip.a")
h_ssl=$(size_of "$WORK_ROOT/shared/ssl.strip.so")
h_crypto=$(size_of "$WORK_ROOT/shared/crypto.strip.so")
s_total=$((s_ssl + s_crypto))
h_total=$((h_ssl + h_crypto))

echo
echo "=== A/B result (armv7hf, stripped) ==="
printf '%-28s %12s %10s\n' artifact bytes MiB
report() { printf '%-28s %12d %10.2f\n' "$1" "$2" "$(awk -v b="$2" 'BEGIN{printf "%.2f", b/1048576}')"; }
report "libssl.a (static)"    "$s_ssl"
report "libcrypto.a (static)" "$s_crypto"
report "STATIC TOTAL"         "$s_total"
report "libssl.so (shared)"   "$h_ssl"
report "libcrypto.so (shared)" "$h_crypto"
report "SHARED TOTAL"         "$h_total"

if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  {
    echo "## BoringSSL A/B (armv7hf, stripped)"
    echo '```'
    echo "STATIC TOTAL: $s_total bytes ($(awk -v b="$s_total" 'BEGIN{printf "%.2f",b/1048576}') MiB)"
    echo "SHARED TOTAL: $h_total bytes ($(awk -v b="$h_total" 'BEGIN{printf "%.2f",b/1048576}') MiB)"
    echo '```'
  } >> "$GITHUB_STEP_SUMMARY"
fi

echo
echo "Note: static .a is the whole archive; the linker GC + Rust LTO pull only"
echo "the symbols meow actually uses, so the real static-link delta in the final"
echo "binary is SMALLER than STATIC TOTAL. .so sizes are the real C-plan cost."
