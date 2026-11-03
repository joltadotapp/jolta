#!/bin/sh
# Builds the `jolta` CLI and signs it with the same Apple Development
# certificate as Jolta.app. A signed binary keeps one identity across rebuilds,
# so a Keychain "Always Allow" for it lasts; cargo's ad-hoc signature changes
# with every build and would ask again each time.
set -eu

cd "$(dirname "$0")/.."
cargo build --release -p jolta-cli

IDENTITY=$(security find-identity -v -p codesigning | awk -F'"' '/Apple Development/ {print $2; exit}')
if [ -z "$IDENTITY" ]; then
    echo "No Apple Development certificate found; target/release/jolta stays ad-hoc signed."
    exit 0
fi
codesign --force --sign "$IDENTITY" --identifier app.jolta.cli --options runtime target/release/jolta
echo "target/release/jolta signed by $IDENTITY"
