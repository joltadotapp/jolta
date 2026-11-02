#!/usr/bin/env bash
# Runs the app's UI journeys (apple/KiteUITests) on the Mac this runs on.
# Each test gets a data folder of its own, filled from apple/KiteUITests/Fixtures
# with the signed kite CLI, because the sandboxed test runner can't write to
# ~/Library/Application Support/Kite. The folders are removed afterwards.
#
#   scripts/ui-tests.sh                     all journeys
#   scripts/ui-tests.sh testOpenConversation
#
# The tests take over the keyboard and mouse while they run. macOS needs
# Automation Mode allowed once per Mac:
#   sudo automationmodetool enable-automationmode-without-authentication

set -euo pipefail

cd "$(dirname "$0")/.."
KITE=target/release/kite
FIXTURES=apple/KiteUITests/Fixtures
DATA="$HOME/Library/Application Support/Kite"
[ -x "$KITE" ] || scripts/build-cli.sh

TESTS=$(sed -n 's/^ *func \(test[A-Za-z0-9]*\)().*/\1/p' apple/KiteUITests/*.swift)
if [ $# -gt 0 ]; then TESTS="$*"; fi

cleanup() {
	for t in $TESTS; do rm -rf "$DATA/UITest-$t"; done
}
trap cleanup EXIT

ONLY=()
for t in $TESTS; do
	dir="$DATA/UITest-$t"
	rm -rf "$dir"
	"$KITE" --data-dir "$dir" dev local-account me@test.invalid >/dev/null
	"$KITE" --data-dir "$dir" import me@test.invalid inbox \
		"$FIXTURES/01-numbers.eml" "$FIXTURES/03-numbers-thanks.eml" "$FIXTURES/04-lunch.eml" \
		"$FIXTURES/05-deals.eml" "$FIXTURES/06-invoice.eml" >/dev/null
	"$KITE" --data-dir "$dir" import me@test.invalid sent "$FIXTURES/02-numbers-reply.eml" >/dev/null
	ONLY+=("-only-testing:KiteUITests/Journeys/$t")
done

xcodebuild -project apple/Kite.xcodeproj -scheme Kite -configuration Debug test "${ONLY[@]}" 2>&1 | tee /tmp/kite-ui-tests.log \
	| grep -E "Test Case .*(passed|failed)|error:|Executed|TEST (SUCCEEDED|FAILED)|\*\* TEST" || true
test "${PIPESTATUS[0]}" -eq 0
