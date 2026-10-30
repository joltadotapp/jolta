#!/bin/sh
# Opens tests/fixtures/hostile.eml in the real Kite.app reader and fails if
# any of its 25 remote resources reaches a local logging server.
#
#   scripts/leak-check.sh [Debug|Release]
#
# Needs a built app and an account to hold the fixture (the first account,
# or KITE_LEAK_ACCOUNT). The app runs in the background and is stopped after.
set -eu

cd "$(dirname "$0")/.."
CONFIG=${1:-Debug}
APP="build/DerivedData/Build/Products/$CONFIG/Kite.app/Contents/MacOS/Kite"
DATA="$HOME/Library/Application Support/Kite"
PORT=18080
LOG=$(mktemp)
KITE=./target/release/kite

[ -x "$APP" ] || { echo "build the app first: no $APP"; exit 2; }
[ -x "$KITE" ] || cargo build -q --release -p kite-cli
ACCOUNT=${KITE_LEAK_ACCOUNT:-$($KITE account list | awk 'NR==1 {print $1}')}
[ -n "$ACCOUNT" ] || { echo "add an account (kite gen works) first"; exit 2; }
$KITE import "$ACCOUNT" inbox tests/fixtures/hostile.eml >/dev/null

python3 "$(dirname "$0")/leak-server.py" "$PORT" 2>"$LOG" &
SERVER=$!
sleep 1
curl -s -o /dev/null "http://127.0.0.1:$PORT/selftest" || true
grep -q selftest "$LOG" || { kill $SERVER; echo "logging server did not start"; exit 2; }

rm -f "$DATA/readerdebug.txt"
"$APP" -ApplePersistenceIgnoreState YES -KiteSearch 'subject:"leak check"' -KiteOpenFirst YES -KiteReaderDebug YES >/dev/null 2>&1 &
APP_PID=$!
for _ in $(seq 1 30); do
    [ -f "$DATA/readerdebug.txt" ] && break
    sleep 1
done
sleep 3
{ kill $APP_PID; kill $SERVER; wait; } 2>/dev/null || true

if ! grep -q "event finished" "$DATA/readerdebug.txt" 2>/dev/null; then
    echo "FAIL: the reader never finished loading the fixture"
    cat "$DATA/readerdebug.txt" 2>/dev/null || true
    exit 1
fi
# Dev tools that watch for new local ports (T3 Code) probe "/" with a Node
# user agent; that request does not come from Kite.
grep -q "\"GET /\" ua='node'" "$LOG" && echo "note: ignored a port probe from a Node process"
HITS=$(grep -v selftest "$LOG" | grep -v "\"GET /\" ua='node'" | grep -c '^"' || true)
if [ "$HITS" -ne 0 ]; then
    echo "FAIL: $HITS request(s) reached the logging server:"
    grep -v selftest "$LOG" | grep -v "\"GET /\" ua='node'"
    exit 1
fi
echo "PASS: hostile message rendered ($(grep probe "$DATA/readerdebug.txt")), 0 requests reached 127.0.0.1:$PORT"
