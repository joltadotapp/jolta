#!/bin/sh
# Builds the Rust core as a static library, generates the UniFFI Swift bindings,
# and packages both into apple/JoltaFFI.
#
#   scripts/build-core.sh            debug build for this Mac's architecture
#   scripts/build-core.sh --release  release build for Apple silicon
#   JOLTA_INTEL=1 scripts/build-core.sh --release   universal (arm64 + x86_64)
#
# Intel is off while iterating: no Mac here can test it (decisions.md).
set -eu

cd "$(dirname "$0")/.."
ROOT=$(pwd)
PKG="$ROOT/apple/JoltaFFI"
LIB=libjolta_ffi.a

if [ "${1:-}" = "--release" ]; then
    PROFILE=release
    TARGETS="aarch64-apple-darwin"
    [ "${JOLTA_INTEL:-0}" = 1 ] && TARGETS="$TARGETS x86_64-apple-darwin"
    CARGO_FLAGS="--release"
else
    PROFILE=debug
    TARGETS="$(uname -m | sed 's/arm64/aarch64/')-apple-darwin"
    CARGO_FLAGS=""
fi

export MACOSX_DEPLOYMENT_TARGET=26.0

# One cargo call per crate builds every target at once.
TARGET_FLAGS=""
for t in $TARGETS; do TARGET_FLAGS="$TARGET_FLAGS --target $t"; done

# shellcheck disable=SC2086
cargo build -p jolta-ffi --lib $TARGET_FLAGS $CARGO_FLAGS
LIBS=""
for t in $TARGETS; do LIBS="$LIBS $ROOT/target/$t/$PROFILE/$LIB"; done

WORK="$ROOT/target/jolta-ffi-swift"
rm -rf "$WORK"
mkdir -p "$WORK/headers" "$WORK/swift" "$WORK/lib"

# shellcheck disable=SC2086
lipo -create $LIBS -output "$WORK/lib/$LIB"

FIRST_LIB=$(echo $LIBS | cut -d' ' -f1)
BINDGEN="cargo run -q -p uniffi-bindgen-swift --"
$BINDGEN --swift-sources "$FIRST_LIB" "$WORK/swift"
$BINDGEN --headers "$FIRST_LIB" "$WORK/headers"
$BINDGEN --modulemap --module-name jolta_ffiFFI --modulemap-filename module.modulemap "$FIRST_LIB" "$WORK/headers"

# Files whose contents didn't change keep their dates, so Xcode doesn't
# recompile the bindings and every Swift file that imports them.
xcodebuild -create-xcframework \
    -library "$WORK/lib/$LIB" -headers "$WORK/headers" \
    -output "$WORK/JoltaFFI.xcframework" >/dev/null
mkdir -p "$PKG/JoltaFFI.xcframework" "$PKG/Sources/JoltaFFI"
rsync -r --checksum --delete "$WORK/JoltaFFI.xcframework/" "$PKG/JoltaFFI.xcframework/"
rsync -r --checksum "$WORK/swift/" "$PKG/Sources/JoltaFFI/"

# jolta-mcp, the stdio-to-socket helper that ships inside Jolta.app.
HELPERS="$ROOT/apple/Helpers"
mkdir -p "$HELPERS"
# shellcheck disable=SC2086
cargo build -p jolta-mcp $TARGET_FLAGS $CARGO_FLAGS
MCP_BINS=""
for t in $TARGETS; do MCP_BINS="$MCP_BINS $ROOT/target/$t/$PROFILE/jolta-mcp"; done
# shellcheck disable=SC2086
lipo -create $MCP_BINS -output "$HELPERS/jolta-mcp"

# jolta-ai, the helper that runs local models. Always optimised: llama.cpp is
# unusably slow in a debug build. Apple silicon only; on Intel the helper
# answers every request with "needs Apple silicon".
# shellcheck disable=SC2086
cargo build -p jolta-ai $TARGET_FLAGS --release
AI_BINS=""
for t in $TARGETS; do AI_BINS="$AI_BINS $ROOT/target/$t/release/jolta-ai"; done
# shellcheck disable=SC2086
lipo -create $AI_BINS -output "$HELPERS/jolta-ai"

echo "JoltaFFI, jolta-mcp and jolta-ai ($PROFILE: $TARGETS) written to $PKG and $HELPERS"
