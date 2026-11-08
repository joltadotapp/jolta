#!/usr/bin/env bash
# Runs the app's UI journeys (apple/JoltaUITests) on the Mac this runs on.
# Each test gets a data folder of its own, filled from apple/JoltaUITests/Fixtures
# with the signed jolta CLI, because the sandboxed test runner can't write to
# ~/Library/Application Support/Jolta. The folders are removed afterwards.
#
#   scripts/ui-tests.sh                     all journeys
#   scripts/ui-tests.sh testOpenConversation
#
# The tests take over the keyboard and mouse while they run. macOS needs
# Automation Mode allowed once per Mac:
#   sudo automationmodetool enable-automationmode-without-authentication

set -euo pipefail

cd "$(dirname "$0")/.."
JOLTA=target/release/jolta
FIXTURES=apple/JoltaUITests/Fixtures
DATA="$HOME/Library/Application Support/Jolta"
[ -x "$JOLTA" ] || scripts/build-cli.sh

TESTS=$(sed -n 's/^ *func \(test[A-Za-z0-9]*\)().*/\1/p' apple/JoltaUITests/*.swift)
if [ $# -gt 0 ]; then TESTS="$*"; fi

cleanup() {
	for t in $TESTS; do rm -rf "$DATA/UITest-$t"; done
}
trap cleanup EXIT

ONLY=()
for t in $TESTS; do
	dir="$DATA/UITest-$t"
	rm -rf "$dir"
	"$JOLTA" --data-dir "$dir" dev local-account me@test.invalid >/dev/null
	"$JOLTA" --data-dir "$dir" import me@test.invalid inbox \
		"$FIXTURES/01-numbers.eml" "$FIXTURES/03-numbers-thanks.eml" "$FIXTURES/04-lunch.eml" \
		"$FIXTURES/05-deals.eml" "$FIXTURES/06-invoice.eml" "$FIXTURES/07-invitation.eml" "$FIXTURES/08-invitation-reply.eml" >/dev/null
	"$JOLTA" --data-dir "$dir" import me@test.invalid sent "$FIXTURES/02-numbers-reply.eml" >/dev/null
	"$JOLTA" --data-dir "$dir" cal import "$FIXTURES/calendar-work.ics" --name Work --color '#4f6fae' >/dev/null
	"$JOLTA" --data-dir "$dir" cal import "$FIXTURES/calendar-personal.ics" --name Personal --color '#9a5a8a' >/dev/null
	if [ "$t" = testMeetingUpdate ] || [ "$t" = testGridDragAndKeys ] || [ "$t" = testAddVideoCall ] || [ "$t" = testMoveBetweenCalendars ]; then
		"$JOLTA" --data-dir "$dir" cal import "$FIXTURES/calendar-team.ics" --name Team --color '#3f8f6b' >/dev/null
		"$JOLTA" --data-dir "$dir" dev calendar-as-google Team me@test.invalid >/dev/null
	fi
	if [ "$t" = testAcceptProposalAndEmailGuests ]; then
		"$JOLTA" --data-dir "$dir" cal import "$FIXTURES/calendar-team.ics" --name Team --color '#3f8f6b' >/dev/null
		"$JOLTA" --data-dir "$dir" dev calendar-as-google Team me@test.invalid >/dev/null
		"$JOLTA" --data-dir "$dir" import me@test.invalid inbox "$FIXTURES/09-counter.eml" >/dev/null
	fi
	if [ "$t" = testAnswerInvitationInCalendar ] || [ "$t" = testSetOutOfOffice ]; then
		"$JOLTA" --data-dir "$dir" dev calendar-as-google Work me@test.invalid >/dev/null
	fi
	if [ "$t" = testFindTimeWithRoom ]; then
		day=$(date -v-7d +%Y%m%d)
		printf 'BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:room-1@test.invalid\r\nDTSTART:%sT090000Z\r\nDTEND:%sT100000Z\r\nSUMMARY:Review\r\nORGANIZER:mailto:me@test.invalid\r\nATTENDEE:mailto:me@test.invalid\r\nATTENDEE;CUTYPE=ROOM;CN=Bruegel:mailto:bruegel@rooms.test.invalid\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n' "$day" "$day" >"$dir/rooms.ics"
		"$JOLTA" --data-dir "$dir" cal import "$dir/rooms.ics" --name Rooms >/dev/null
		"$JOLTA" --data-dir "$dir" dev calendar-as-google Rooms me@test.invalid >/dev/null
		printf '{"people": {"bruegel@rooms.test.invalid": {"known": []}}}' >"$dir/free-busy.json"
	fi
	if [ "$t" = testFindTimeInvite ]; then
		"$JOLTA" --data-dir "$dir" import me@test.invalid inbox "$FIXTURES/10-planning.eml" >/dev/null
		day=$(date -v-7d +%Y%m%d)
		printf 'BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:room-1@test.invalid\r\nDTSTART:%sT090000Z\r\nDTEND:%sT100000Z\r\nSUMMARY:Review\r\nORGANIZER:mailto:me@test.invalid\r\nATTENDEE:mailto:me@test.invalid\r\nATTENDEE;CUTYPE=ROOM;CN=Bruegel:mailto:bruegel@rooms.test.invalid\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n' "$day" "$day" >"$dir/rooms.ics"
		"$JOLTA" --data-dir "$dir" cal import "$dir/rooms.ics" --name Rooms >/dev/null
		"$JOLTA" --data-dir "$dir" dev calendar-as-google Rooms me@test.invalid >/dev/null
		printf '{"people": {"ann@test.invalid": {"known": []}, "bob@test.invalid": {"known": []}, "carl@test.invalid": {"known": []}, "bruegel@rooms.test.invalid": {"known": []}}}' >"$dir/free-busy.json"
	fi
	if [ "$t" = testTurnOffAutomaticReplies ]; then
		printf '{"me@test.invalid": {"automaticRepliesSetting": {"status": "alwaysEnabled", "externalAudience": "all"}}}' >"$dir/away.json"
	fi
	ONLY+=("-only-testing:JoltaUITests/Journeys/$t")
done

xcodebuild -project apple/Jolta.xcodeproj -scheme Jolta -configuration Debug test "${ONLY[@]}" 2>&1 | tee /tmp/jolta-ui-tests.log \
	| grep -E "Test Case .*(passed|failed)|error:|Executed|TEST (SUCCEEDED|FAILED)|\*\* TEST" || true
test "${PIPESTATUS[0]}" -eq 0
