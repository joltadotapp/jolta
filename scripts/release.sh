#!/bin/sh
# Builds a universal Release Jolta.app signed with the Developer ID certificate,
# zips it into the update folder and regenerates the Sparkle appcast there.
#
#   scripts/release.sh 0.1.1
#
# The update folder (~/jolta-updates/mac) is served on the tailnet: the launch agent
# app.jolta.updates serves ~/jolta-updates on 127.0.0.1:8447, and tailscale serve
# publishes that on port 8448. Kite's old feed stays at the folder's root.
# The appcast is signed with the EdDSA key in ~/jolta-devid; its public half is
# SUPublicEDKey in apple/Jolta-Info.plist. Not notarized during the MVP.
set -euo pipefail

VERSION=${1:?usage: scripts/release.sh <version>}
BUILD=$(date +%Y%m%d%H%M)
UPDATES=${JOLTA_UPDATES:-$HOME/jolta-updates/mac}
SPARKLE_BIN=${SPARKLE_BIN:-$HOME/jolta-tools-sparkle/bin}
ED_KEY=${ED_KEY:-$HOME/jolta-devid/sparkle-ed25519-private.key}
# The feed's address is on the tailnet, so it stays out of the repository:
# one line such as https://<host>.<tailnet>.ts.net:8448/mac/ in this file.
FEED_FILE=${JOLTA_FEED_FILE:-$HOME/jolta-devid/feed-url}
[ -s "$FEED_FILE" ] || { echo "No feed address in $FEED_FILE."; exit 1; }
FEED_PREFIX=$(tr -d '[:space:]' < "$FEED_FILE")

cd "$(dirname "$0")/.."
ROOT=$(pwd)
IDENTITY=$(security find-identity -v -p codesigning | awk -F'"' '/Developer ID Application/ {print $2; exit}')
[ -n "$IDENTITY" ] || { echo "No Developer ID Application certificate in the Keychain."; exit 1; }
[ -f "$ED_KEY" ] || { echo "No Sparkle signing key at $ED_KEY."; exit 1; }
# A published version is never rebuilt: its appcast entry is signed for the old zip.
[ ! -e "$UPDATES/Jolta-$VERSION.zip" ] || { echo "Jolta $VERSION is already published; pick a new version."; exit 1; }

scripts/build-core.sh --release

DERIVED="$ROOT/target/xcode-release"
xcodebuild -project apple/Jolta.xcodeproj -scheme Jolta -configuration Release \
    -derivedDataPath "$DERIVED" \
    ARCHS="arm64 x86_64" ONLY_ACTIVE_ARCH=NO \
    MARKETING_VERSION="$VERSION" CURRENT_PROJECT_VERSION="$BUILD" JOLTA_FEED_URL="${FEED_PREFIX}appcast.xml" \
    CODE_SIGN_STYLE=Manual CODE_SIGN_IDENTITY="$IDENTITY" DEVELOPMENT_TEAM=RWQLHW7V7V \
    OTHER_CODE_SIGN_FLAGS=--timestamp CODE_SIGN_INJECT_BASE_ENTITLEMENTS=NO \
    build | grep -E "error:|BUILD (SUCCEEDED|FAILED)"

APP="$DERIVED/Build/Products/Release/Jolta.app"
# A build without the feed address could never update itself again.
FEED_IN_APP=$(/usr/libexec/PlistBuddy -c "Print :SUFeedURL" "$APP/Contents/Info.plist")
[ "$FEED_IN_APP" = "${FEED_PREFIX}appcast.xml" ] || { echo "The app's SUFeedURL is '$FEED_IN_APP', not the feed."; exit 1; }

# Xcode re-signs Sparkle.framework but not the helpers inside it; sign them
# inside-out with our identity, as Sparkle's documentation describes.
SPARKLE="$APP/Contents/Frameworks/Sparkle.framework/Versions/B"
sign() { codesign --force --timestamp --options runtime --sign "$IDENTITY" "$@"; }
sign "$SPARKLE/XPCServices/Installer.xpc"
sign --preserve-metadata=entitlements "$SPARKLE/XPCServices/Downloader.xpc"
sign "$SPARKLE/Autoupdate"
sign "$SPARKLE/Updater.app"
sign "$APP/Contents/Frameworks/Sparkle.framework"
sign --entitlements apple/jolta-mcp.entitlements "$APP/Contents/MacOS/jolta-mcp"
sign --entitlements apple/jolta-ai.entitlements "$APP/Contents/MacOS/jolta-ai"
sign --preserve-metadata=entitlements "$APP"
codesign --verify --deep --strict "$APP"

mkdir -p "$UPDATES"
ZIP="$UPDATES/Jolta-$VERSION.zip"
ditto -c -k --sequesterRsrc --keepParent "$APP" "$ZIP"

"$SPARKLE_BIN/generate_appcast" --ed-key-file "$ED_KEY" --download-url-prefix "$FEED_PREFIX" "$UPDATES"

echo "Jolta $VERSION (build $BUILD) published: ${FEED_PREFIX}appcast.xml"
