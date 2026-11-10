#!/bin/sh
# Checks the Developer ID provisioning profile that release builds embed, before anything is
# built: it must be for app.jolta.mac, allow Time Sensitive notifications and not have expired.
# Without such a profile, macOS refuses to launch a Developer ID build that carries the
# Time Sensitive entitlement ("No matching profile found", measured 2026-11-10).
#
#   scripts/check-profile.sh ~/jolta-devid/Jolta_Developer_ID.provisionprofile
set -eu

PROFILE=${1:?usage: scripts/check-profile.sh <profile>}
APP_ID=RWQLHW7V7V.app.jolta.mac

fail() {
    echo "$1"
    cat <<EOF
Release builds need a Developer ID provisioning profile so urgent mail and meeting reminders can
use Time Sensitive notifications. Make one at developer.apple.com/account, in the team RWQLHW7V7V:
  1. Certificates, Identifiers & Profiles > Identifiers: open the App ID app.jolta.mac (or create
     an explicit macOS App ID with that bundle ID), tick Time Sensitive Notifications under
     Capabilities, and Save.
  2. Profiles > +: Distribution > Developer ID, App ID app.jolta.mac, the Developer ID Application
     certificate this Mac signs with, name it "Jolta Developer ID", Generate, and Download.
  3. Save the downloaded file as $PROFILE.
EOF
    exit 1
}

[ -f "$PROFILE" ] || fail "No provisioning profile at $PROFILE."
PLIST=$(mktemp)
trap 'rm -f "$PLIST"' EXIT
security cms -D -i "$PROFILE" > "$PLIST" 2>/dev/null || fail "$PROFILE isn't a provisioning profile."
value() { /usr/libexec/PlistBuddy -c "Print :$1" "$PLIST" 2>/dev/null || true; }

ID=$(value Entitlements:com.apple.application-identifier)
[ "$ID" = "$APP_ID" ] || fail "$PROFILE is for '$ID', not $APP_ID."
[ "$(value ProvisionsAllDevices)" = true ] || fail "$PROFILE isn't a Developer ID profile."
[ "$(value Entitlements:com.apple.developer.usernotifications.time-sensitive)" = true ] \
    || fail "$PROFILE doesn't allow Time Sensitive notifications; tick the capability and download the profile again."
EXPIRES=$(plutil -extract ExpirationDate raw "$PLIST")
[ "$EXPIRES" \> "$(date -u +%Y-%m-%dT%H:%M:%SZ)" ] || fail "$PROFILE expired on $EXPIRES."
echo "Provisioning profile for $APP_ID, valid until $EXPIRES."
