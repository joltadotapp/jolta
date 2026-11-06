#!/bin/sh
# Builds a Release Jolta.app (Apple silicon, or universal with JOLTA_INTEL=1) signed with the
# Developer ID certificate and notarized, zips it into the update folder and regenerates the
# Sparkle appcast there.
#
#   scripts/release.sh 0.4.1                  test build for Koen's Macs, notarized
#   scripts/release.sh 0.4.1 --no-notarize    test build, a few minutes faster
#   scripts/release.sh 0.4.0 --public         public release on jolta.app
#
# A test build's feed is the private one on the tailnet: the launch agent app.jolta.updates
# serves ~/jolta-updates on 127.0.0.1:8447, and tailscale serve publishes that on port 8448.
# Its address is in ~/jolta-devid/feed-url, one line such as https://<host>.<tailnet>.ts.net:8448/mac/.
# A test build also carries JoltaTestBuild, so the Mac remembers that feed (Updater.swift).
#
# A public release's feed is https://jolta.app/updates/mac/appcast.xml. Its zips are kept in
# ~/jolta-updates/public/mac and copied to the web server named in ~/jolta-devid/site-target
# (an rsync destination such as user@host:jolta/web). The latest one is also
# https://jolta.app/download/Jolta.zip. A public release goes into the test feed too.
#
# Both appcasts are signed with the EdDSA key in ~/jolta-devid; its public half is SUPublicEDKey
# in apple/Jolta-Info.plist. The OAuth clients the app carries come from
# ~/jolta-devid/oauth-clients.env, in the same form as the data folder's file.
# Notarization uses the notarytool keychain profile jolta-notary.
set -euo pipefail

VERSION=${1:?usage: scripts/release.sh <version> [--public | --no-notarize]}
shift
PUBLIC=0
NOTARIZE=1
for arg in "$@"; do
    case $arg in
        --public) PUBLIC=1 ;;
        --no-notarize) NOTARIZE=0 ;;
        *) echo "Unknown option $arg"; exit 1 ;;
    esac
done
[ $PUBLIC = 0 ] || [ $NOTARIZE = 1 ] || { echo "A public release is always notarized."; exit 1; }

BUILD=$(date +%Y%m%d%H%M)
DEVID=$HOME/jolta-devid
UPDATES=${JOLTA_UPDATES:-$HOME/jolta-updates/mac}
PUBLIC_UPDATES=${JOLTA_PUBLIC_UPDATES:-$HOME/jolta-updates/public/mac}
PUBLIC_URL=https://jolta.app/updates/mac/
SPARKLE_BIN=${SPARKLE_BIN:-$HOME/jolta-tools-sparkle/bin}
ED_KEY=${ED_KEY:-$DEVID/sparkle-ed25519-private.key}
FEED_FILE=${JOLTA_FEED_FILE:-$DEVID/feed-url}
CLIENTS=${JOLTA_CLIENTS_FILE:-$DEVID/oauth-clients.env}
[ -s "$FEED_FILE" ] || { echo "No feed address in $FEED_FILE."; exit 1; }
TEST_URL=$(tr -d '[:space:]' < "$FEED_FILE")

if [ $PUBLIC = 1 ]; then
    SITE=$(tr -d '[:space:]' < "$DEVID/site-target" 2>/dev/null || true)
    [ -n "$SITE" ] || { echo "No web server in $DEVID/site-target."; exit 1; }
    FEED_URL=$PUBLIC_URL
    TEST_BUILD=
else
    FEED_URL=$TEST_URL
    TEST_BUILD=YES
fi

client() { sed -n "s/^$1=//p" "$CLIENTS" 2>/dev/null | tr -d '[:space:]'; }
JOLTA_GOOGLE_CLIENT_ID=$(client GOOGLE_CLIENT_ID)
JOLTA_GOOGLE_CLIENT_SECRET=$(client GOOGLE_CLIENT_SECRET)
JOLTA_MICROSOFT_CLIENT_ID=$(client MICROSOFT_CLIENT_ID)
[ -n "$JOLTA_GOOGLE_CLIENT_ID" ] && [ -n "$JOLTA_GOOGLE_CLIENT_SECRET" ] && [ -n "$JOLTA_MICROSOFT_CLIENT_ID" ] \
    || { echo "$CLIENTS needs GOOGLE_CLIENT_ID, GOOGLE_CLIENT_SECRET and MICROSOFT_CLIENT_ID."; exit 1; }
export JOLTA_GOOGLE_CLIENT_ID JOLTA_GOOGLE_CLIENT_SECRET JOLTA_MICROSOFT_CLIENT_ID

cd "$(dirname "$0")/.."
ROOT=$(pwd)
IDENTITY=$(security find-identity -v -p codesigning | awk -F'"' '/Developer ID Application/ {print $2; exit}')
[ -n "$IDENTITY" ] || { echo "No Developer ID Application certificate in the Keychain."; exit 1; }
[ -f "$ED_KEY" ] || { echo "No Sparkle signing key at $ED_KEY."; exit 1; }
# A published version is never rebuilt: its appcast entry is signed for the old zip.
[ ! -e "$UPDATES/Jolta-$VERSION.zip" ] || { echo "Jolta $VERSION is already published; pick a new version."; exit 1; }
[ ! -e "$PUBLIC_UPDATES/Jolta-$VERSION.zip" ] || { echo "Jolta $VERSION is already public; pick a new version."; exit 1; }

# Apple silicon only while iterating; JOLTA_INTEL=1 builds the universal app.
ARCHS=arm64
[ "${JOLTA_INTEL:-0}" = 1 ] && ARCHS="arm64 x86_64"
scripts/build-core.sh --release

DERIVED="$ROOT/target/xcode-release"
xcodebuild -project apple/Jolta.xcodeproj -scheme Jolta -configuration Release \
    -derivedDataPath "$DERIVED" \
    ARCHS="$ARCHS" ONLY_ACTIVE_ARCH=NO \
    MARKETING_VERSION="$VERSION" CURRENT_PROJECT_VERSION="$BUILD" \
    JOLTA_FEED_URL="${FEED_URL}appcast.xml" JOLTA_TEST_BUILD="$TEST_BUILD" \
    CODE_SIGN_STYLE=Manual CODE_SIGN_IDENTITY="$IDENTITY" DEVELOPMENT_TEAM=RWQLHW7V7V \
    OTHER_CODE_SIGN_FLAGS=--timestamp CODE_SIGN_INJECT_BASE_ENTITLEMENTS=NO \
    build | grep -E "error:|BUILD (SUCCEEDED|FAILED)"

APP="$DERIVED/Build/Products/Release/Jolta.app"
# A build without the feed address could never update itself again.
FEED_IN_APP=$(/usr/libexec/PlistBuddy -c "Print :SUFeedURL" "$APP/Contents/Info.plist")
[ "$FEED_IN_APP" = "${FEED_URL}appcast.xml" ] || { echo "The app's SUFeedURL is '$FEED_IN_APP', not the feed."; exit 1; }
# A build without the OAuth clients can't add a Gmail or Microsoft 365 account on a new Mac.
grep -rqaF "$JOLTA_GOOGLE_CLIENT_ID" "$APP/Contents/MacOS" "$APP/Contents/Frameworks" \
    || { echo "The app doesn't carry the Google OAuth client."; exit 1; }

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

if [ $NOTARIZE = 1 ]; then
    WORK=$(mktemp -d)
    ditto -c -k --keepParent "$APP" "$WORK/Jolta.zip"
    echo "Notarizing; Apple usually answers within a few minutes."
    xcrun notarytool submit "$WORK/Jolta.zip" --keychain-profile jolta-notary --wait --timeout 30m \
        --output-format json > "$WORK/result.json" || true
    STATUS=$(plutil -extract status raw "$WORK/result.json" 2>/dev/null || echo unknown)
    if [ "$STATUS" != Accepted ]; then
        echo "Notarization: $STATUS"
        ID=$(plutil -extract id raw "$WORK/result.json" 2>/dev/null || true)
        [ -z "$ID" ] || xcrun notarytool log "$ID" --keychain-profile jolta-notary
        exit 1
    fi
    xcrun stapler staple "$APP"
    spctl --assess --type execute "$APP"
    rm -rf "$WORK"
fi

mkdir -p "$UPDATES"
ZIP="$UPDATES/Jolta-$VERSION.zip"
ditto -c -k --sequesterRsrc --keepParent "$APP" "$ZIP"
"$SPARKLE_BIN/generate_appcast" --ed-key-file "$ED_KEY" --download-url-prefix "$TEST_URL" "$UPDATES"

if [ $PUBLIC = 1 ]; then
    mkdir -p "$PUBLIC_UPDATES"
    cp "$ZIP" "$PUBLIC_UPDATES/"
    "$SPARKLE_BIN/generate_appcast" --ed-key-file "$ED_KEY" --download-url-prefix "$PUBLIC_URL" "$PUBLIC_UPDATES"
    rsync -a "$PUBLIC_UPDATES/" "$SITE/updates/mac/"
    rsync -a "$ZIP" "$SITE/download/Jolta.zip"
    curl -fsS "${PUBLIC_URL}appcast.xml" | grep "sparkle:shortVersionString>$VERSION<" > /dev/null \
        || { echo "jolta.app doesn't serve $VERSION in its appcast."; exit 1; }
    curl -fsSI https://jolta.app/download/Jolta.zip > /dev/null \
        || { echo "jolta.app doesn't serve the download."; exit 1; }
    echo "Jolta $VERSION (build $BUILD) is public: ${PUBLIC_URL}appcast.xml and https://jolta.app/download/Jolta.zip"
else
    echo "Jolta $VERSION (build $BUILD) published to the test feed: ${TEST_URL}appcast.xml"
fi
