#!/bin/sh
# Builds a universal Release Kite.app signed with the Developer ID certificate,
# zips it into the update folder and regenerates the Sparkle appcast there.
#
#   scripts/release.sh 0.1.1
#
# The update folder (~/kite-updates) is served on the tailnet by tailscale serve.
# The appcast is signed with the EdDSA key in ~/kite-devid; its public half is
# SUPublicEDKey in apple/Kite-Info.plist. Not notarized during the MVP.
set -eu

VERSION=${1:?usage: scripts/release.sh <version>}
BUILD=$(date +%Y%m%d%H%M)
UPDATES=${KITE_UPDATES:-$HOME/kite-updates}
SPARKLE_BIN=${SPARKLE_BIN:-$HOME/kite-tools-sparkle/bin}
ED_KEY=${ED_KEY:-$HOME/kite-devid/sparkle-ed25519-private.key}
FEED_PREFIX=https://mac-mini.example.ts.net:8448/

cd "$(dirname "$0")/.."
ROOT=$(pwd)
IDENTITY=$(security find-identity -v -p codesigning | awk -F'"' '/Developer ID Application/ {print $2; exit}')
[ -n "$IDENTITY" ] || { echo "No Developer ID Application certificate in the Keychain."; exit 1; }
[ -f "$ED_KEY" ] || { echo "No Sparkle signing key at $ED_KEY."; exit 1; }
# A published version is never rebuilt: its appcast entry is signed for the old zip.
[ ! -e "$UPDATES/Kite-$VERSION.zip" ] || { echo "Kite $VERSION is already published; pick a new version."; exit 1; }

scripts/build-core.sh --release

DERIVED="$ROOT/target/xcode-release"
xcodebuild -project apple/Kite.xcodeproj -scheme Kite -configuration Release \
    -derivedDataPath "$DERIVED" \
    ARCHS="arm64 x86_64" ONLY_ACTIVE_ARCH=NO \
    MARKETING_VERSION="$VERSION" CURRENT_PROJECT_VERSION="$BUILD" \
    CODE_SIGN_STYLE=Manual CODE_SIGN_IDENTITY="$IDENTITY" DEVELOPMENT_TEAM=RWQLHW7V7V \
    OTHER_CODE_SIGN_FLAGS=--timestamp CODE_SIGN_INJECT_BASE_ENTITLEMENTS=NO \
    build | grep -E "error:|BUILD (SUCCEEDED|FAILED)"

APP="$DERIVED/Build/Products/Release/Kite.app"

# Xcode re-signs Sparkle.framework but not the helpers inside it; sign them
# inside-out with our identity, as Sparkle's documentation describes.
SPARKLE="$APP/Contents/Frameworks/Sparkle.framework/Versions/B"
sign() { codesign --force --timestamp --options runtime --sign "$IDENTITY" "$@"; }
sign "$SPARKLE/XPCServices/Installer.xpc"
sign --preserve-metadata=entitlements "$SPARKLE/XPCServices/Downloader.xpc"
sign "$SPARKLE/Autoupdate"
sign "$SPARKLE/Updater.app"
sign "$APP/Contents/Frameworks/Sparkle.framework"
sign --entitlements apple/kite-mcp.entitlements "$APP/Contents/MacOS/kite-mcp"
sign --preserve-metadata=entitlements "$APP"
codesign --verify --deep --strict "$APP"

mkdir -p "$UPDATES"
ZIP="$UPDATES/Kite-$VERSION.zip"
ditto -c -k --sequesterRsrc --keepParent "$APP" "$ZIP"

"$SPARKLE_BIN/generate_appcast" --ed-key-file "$ED_KEY" --download-url-prefix "$FEED_PREFIX" "$UPDATES"

echo "Kite $VERSION (build $BUILD) published: ${FEED_PREFIX}appcast.xml"
