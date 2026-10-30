#!/bin/sh
# Builds the Rust core as a static library, generates the UniFFI Swift bindings,
# and packages both into apple/KiteFFI.
#
#   scripts/build-core.sh            debug build for this Mac's architecture
#   scripts/build-core.sh --release  release build, universal (arm64 + x86_64)
set -eu

cd "$(dirname "$0")/.."
ROOT=$(pwd)
PKG="$ROOT/apple/KiteFFI"
LIB=libkite_ffi.a

if [ "${1:-}" = "--release" ]; then
    PROFILE=release
    TARGETS="aarch64-apple-darwin x86_64-apple-darwin"
    CARGO_FLAGS="--release"
else
    PROFILE=debug
    TARGETS="$(uname -m | sed 's/arm64/aarch64/')-apple-darwin"
    CARGO_FLAGS=""
fi

export MACOSX_DEPLOYMENT_TARGET=26.0

LIBS=""
for t in $TARGETS; do
    cargo build -p kite-ffi --lib --target "$t" $CARGO_FLAGS
    LIBS="$LIBS $ROOT/target/$t/$PROFILE/$LIB"
done

WORK="$ROOT/target/kite-ffi-swift"
rm -rf "$WORK"
mkdir -p "$WORK/headers" "$WORK/swift" "$WORK/lib"

# shellcheck disable=SC2086
lipo -create $LIBS -output "$WORK/lib/$LIB"

FIRST_LIB=$(echo $LIBS | cut -d' ' -f1)
BINDGEN="cargo run -q -p kite-ffi --bin uniffi-bindgen-swift --"
$BINDGEN --swift-sources "$FIRST_LIB" "$WORK/swift"
$BINDGEN --headers "$FIRST_LIB" "$WORK/headers"
$BINDGEN --modulemap --module-name kite_ffiFFI --modulemap-filename module.modulemap "$FIRST_LIB" "$WORK/headers"

rm -rf "$PKG/KiteFFI.xcframework"
xcodebuild -create-xcframework \
    -library "$WORK/lib/$LIB" -headers "$WORK/headers" \
    -output "$PKG/KiteFFI.xcframework" >/dev/null

mkdir -p "$PKG/Sources/KiteFFI"
cp "$WORK/swift/"*.swift "$PKG/Sources/KiteFFI/"

echo "KiteFFI ($PROFILE: $TARGETS) written to $PKG"
