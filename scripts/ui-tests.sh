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
TESTS=("$@")
if [ "$#" -eq 0 ]; then
	TESTS=($(sed -n 's/^ *func \(test[A-Za-z0-9]*\)().*/\1/p' apple/JoltaUITests/*.swift))
fi
if [ -z "${TESTS[*]-}" ]; then
	echo "No Jolta UI test journeys selected." >&2
	exit 1
fi
for t in "${TESTS[@]}"; do
	if ! [[ "$t" =~ ^test[A-Za-z0-9]+$ ]]; then
		echo "Invalid Jolta UI test name: $t" >&2
		exit 1
	fi
done
[ -x "$JOLTA" ] || scripts/build-cli.sh

WATCHERS=()
SECRETS=()
cleanup() {
	for pid in ${WATCHERS[@]+"${WATCHERS[@]}"}; do kill "$pid" 2>/dev/null || true; done
	for ref in ${SECRETS[@]+"${SECRETS[@]}"}; do security delete-generic-password -s app.jolta.mac -a "$ref" >/dev/null 2>&1 || true; done
	for t in "${TESTS[@]}"; do rm -rf "$DATA/UITest-$t"; done
}
trap cleanup EXIT

ONLY=()
for t in "${TESTS[@]}"; do
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
	if [ "$t" = testAddZoomLink ]; then
		"$JOLTA" --data-dir "$dir" cal zoom-link https://zoom.us/j/4155550123 >/dev/null
	fi
	if [ "$t" = testAcceptProposalAndEmailGuests ]; then
		"$JOLTA" --data-dir "$dir" cal import "$FIXTURES/calendar-team.ics" --name Team --color '#3f8f6b' >/dev/null
		"$JOLTA" --data-dir "$dir" dev calendar-as-google Team me@test.invalid >/dev/null
		"$JOLTA" --data-dir "$dir" import me@test.invalid inbox "$FIXTURES/09-counter.eml" >/dev/null
	fi
	if [ "$t" = testAnswerInvitationInCalendar ] || [ "$t" = testSetOutOfOffice ] || [ "$t" = testFocusTime ]; then
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
	if [ "$t" = testInsertAvailabilityOnTheirClock ]; then
		"$JOLTA" --data-dir "$dir" import me@test.invalid inbox "$FIXTURES/11-new-york.eml" >/dev/null
	fi
	if [ "$t" = testHoldAndRelease ]; then
		"$JOLTA" --data-dir "$dir" import me@test.invalid inbox "$FIXTURES/11-new-york.eml" >/dev/null
		"$JOLTA" --data-dir "$dir" import me@test.invalid sent "$FIXTURES/12-holds-sent.eml" >/dev/null
		# Two days ahead that are Monday, Tuesday or Friday: the Work calendar's all-day summit takes Wednesday and Thursday.
		days=()
		for n in 1 2 3 4 5 6 7 8 9 10; do
			case $(date -v+${n}d +%u) in 1|2|5) days+=("$(date -v+${n}d +%Y-%m-%d)") ;; esac
		done
		"$JOLTA" --data-dir "$dir" cal hold me@test.invalid --to 'Ann Berg <ann@test.invalid>' --subject 'Partnership call' --message-id '<ui-12@test.invalid>' \
			--slot "${days[0]} 07:00" --slot "${days[1]} 07:00" >/dev/null
	fi
	if [ "$t" = testSendFromAnAlias ]; then
		"$JOLTA" --data-dir "$dir" dev local-account other@test.invalid >/dev/null
		sqlite3 "$dir/jolta.sqlite" "UPDATE account SET smtp_host = 'smtp.test.invalid'"
	fi
	if [ "$t" = testNewMailAtInboxZeroStaysUnread ]; then
		# An account on the local Dovecot (scripts/test-servers.sh up). The CLI's
		# Keychain item goes again: the journey enters the password in Settings so
		# Jolta makes its own, which it reads without a dialog.
		server="inbox-zero-$(date +%s)@test.invalid"
		if echo jolta-test | "$JOLTA" --data-dir "$dir" account add "$server" --imap-host 127.0.0.1 --imap-port 31143 \
			--smtp-host 127.0.0.1 --smtp-port 31025 --password-stdin >/dev/null 2>&1; then
			ref=$(sqlite3 "$dir/jolta.sqlite" "SELECT secret_ref FROM account WHERE address = '$server'")
			security delete-generic-password -s app.jolta.mac -a "$ref" >/dev/null 2>&1
			SECRETS+=("$ref")
			printf 'From: Ann Berg <ann@test.invalid>\r\nTo: <%s>\r\nSubject: Quarterly plan\r\nDate: %s\r\nMessage-ID: <%s.plan@test.invalid>\r\n\r\nCan you look at the plan today?\r\n' \
				"$server" "$(date -R)" "$(date +%s)" >"$dir/new-mail.eml"
			# Delivers the message once the journey has emptied the inbox.
			(
				until [ "$(sqlite3 "$dir/jolta.sqlite" "SELECT count(*) FROM message_mailbox mm JOIN mailbox b ON b.id = mm.mailbox_id WHERE b.role = 'inbox'" 2>/dev/null)" = 0 ]; do
					sleep 0.5
				done
				python3 -I -c 'import imaplib, sys
m = imaplib.IMAP4("127.0.0.1", 31143)
m.login(sys.argv[1], sys.argv[2])
m.append("INBOX", "", None, open(sys.argv[3], "rb").read())' "$server" jolta-test "$dir/new-mail.eml"
			) &
			WATCHERS+=($!)
		fi
	fi
	if [ "$t" = testNewReplyShowsInOpenConversation ]; then
		# A long unread message on the local Dovecot, and a reply delivered once
		# the journey has opened it and Jolta has marked it read.
		server="new-reply-$(date +%s)@test.invalid"
		if echo jolta-test | "$JOLTA" --data-dir "$dir" account add "$server" --imap-host 127.0.0.1 --imap-port 31143 \
			--smtp-host 127.0.0.1 --smtp-port 31025 --password-stdin >/dev/null 2>&1; then
			ref=$(sqlite3 "$dir/jolta.sqlite" "SELECT secret_ref FROM account WHERE address = '$server'")
			security delete-generic-password -s app.jolta.mac -a "$ref" >/dev/null 2>&1
			SECRETS+=("$ref")
			deliver='import imaplib, sys, email.utils
user, kind = sys.argv[1], sys.argv[2]
if kind == "first":
    head = "From: Ann Berg <ann@test.invalid>\r\nSubject: Spring order terms\r\nMessage-ID: <spring-1@test.invalid>\r\n"
    body = "\r\n".join(f"Paragraph {i}: the supplier sent the updated terms for the spring order." for i in range(1, 80))
else:
    head = "From: Bo Nilsson <bo@test.invalid>\r\nSubject: Re: Spring order terms\r\nMessage-ID: <spring-2@test.invalid>\r\nIn-Reply-To: <spring-1@test.invalid>\r\nReferences: <spring-1@test.invalid>\r\n"
    body = "Thanks, I will send the signed copy tomorrow."
raw = f"{head}To: <{user}>\r\nDate: {email.utils.formatdate(localtime=True)}\r\n\r\n{body}\r\n"
m = imaplib.IMAP4("127.0.0.1", 31143)
m.login(user, "jolta-test")
m.append("INBOX", "", None, raw.encode())'
			python3 -I -c "$deliver" "$server" first
			(
				until [ "$(sqlite3 "$dir/jolta.sqlite" "SELECT flags & 1 FROM message WHERE subject = 'Spring order terms'" 2>/dev/null)" = 1 ]; do
					sleep 0.5
				done
				python3 -I -c "$deliver" "$server" reply
			) &
			WATCHERS+=($!)
		fi
	fi
	if [ "$t" = testWideImageFitsAndZooms ] || [ "$t" = testWideLayoutFitsTheReader ]; then
		"$JOLTA" --data-dir "$dir" import me@test.invalid inbox "$FIXTURES/14-wide-screenshot.eml" "$FIXTURES/15-wide-layout.eml" >/dev/null
	fi
	if [ "$t" = testLoadedImagesStayLoaded ]; then
		"$JOLTA" --data-dir "$dir" import me@test.invalid inbox "$FIXTURES/17-autumn-images.eml" "$FIXTURES/18-winter-images.eml" >/dev/null
	fi
	if [ "$t" = testWideImageInScaledMailFitsAndZooms ]; then
		"$JOLTA" --data-dir "$dir" import me@test.invalid inbox "$FIXTURES/16-wide-screenshot-wide-signature.eml" >/dev/null
	fi
	if [ "$t" = testSummaryShowsNoLinks ]; then
		sqlite3 "$dir/jolta.sqlite" "INSERT INTO ai_summary (thread_id, last_message, model, text, at)
			SELECT thread_id, id, 'fixture', '- Bob asks to pay at [view invoice](http://127.0.0.1:9/summary-link).' || char(10) || '- The menu is at <https://example.test/menu>.', 0
			FROM message WHERE subject = 'Lunch on Friday'"
	fi
	if [ "$t" = testComposeFocusesWhereYouType ]; then
		"$JOLTA" --data-dir "$dir" import me@test.invalid drafts "$FIXTURES/13-draft.eml" >/dev/null
	fi
	if [ "$t" = testTurnOffAutomaticReplies ]; then
		printf '{"me@test.invalid": {"automaticRepliesSetting": {"status": "alwaysEnabled", "externalAudience": "all"}}}' >"$dir/away.json"
	fi
	if [ "$t" = testGmailAutomaticRepliesOpenGmail ]; then
		sqlite3 "$dir/jolta.sqlite" "UPDATE account SET kind = 'gmail'"
		printf '{"me@test.invalid": {"vacation": {"enableAutoReply": true, "responseBodyPlainText": "Back on Monday."}}}' >"$dir/away.json"
	fi
	ONLY+=("-only-testing:JoltaUITests/Journeys/$t")
done

LOG=$(mktemp /tmp/jolta-ui-tests.XXXXXX)
echo "Full UI test log is in $LOG"
status=0
if xcodebuild -project apple/Jolta.xcodeproj -scheme Jolta -configuration Debug test "${ONLY[@]}" 2>&1 | tee "$LOG" \
	| { grep -E "Test Case .*(passed|failed|skipped)|error:|Executed|TEST (SUCCEEDED|FAILED)|\*\* TEST" || true; }; then
	:
else
	statuses=("${PIPESTATUS[@]}")
	status=${statuses[0]}
	[ "$status" -ne 0 ] || status=${statuses[1]}
fi

# xcodebuild can report success when a journey was skipped or never ran.
for t in "${TESTS[@]}"; do
	if ! awk -v prefix="Test Case '-[JoltaUITests.Journeys $t]' passed (" 'index($0, prefix) == 1 { found = 1; exit } END { exit !found }' "$LOG"; then
		echo "$t did not pass; the full log is in $LOG" >&2
		[ "$status" -ne 0 ] || status=1
	fi
done
exit "$status"
