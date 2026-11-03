#!/usr/bin/env bash
# Checks sync, actions, undo and sending against real accounts, for the
# providers the local Dovecot can't stand in for (Gmail, Microsoft 365) and
# for iCloud. Run by hand only:
#
#   scripts/live-check.sh gmail-test@example.com m365-test@example.com you@icloud.com
#
# Safety, because these are real mailboxes:
# - It works in its own data folder (Jolta/LiveCheck) holding only the named
#   accounts, copied from the real store with the same Keychain entries, so
#   no other account is synced or touched.
# - It refuses an account with more than 500 messages, which looks like a
#   mailbox in daily use.
# - It only acts on mail it sends itself, to the account's own address, with
#   a random token in the subject.
# - It deletes nothing for good: its own messages end in Trash.
# - It records every other message (folders and flags) before and compares
#   after; any difference fails the check.

set -euo pipefail

cd "$(dirname "$0")/.."
REAL="$HOME/Library/Application Support/Jolta"
DIR="$REAL/LiveCheck"
JOLTA="target/release/jolta"
TOKEN="jolta-live-$(date +%Y%m%d%H%M%S)-$RANDOM"
[ $# -gt 0 ] || { echo "usage: $0 <account address>..." >&2; exit 2; }
[ -x "$JOLTA" ] || scripts/build-cli.sh

fail() { echo "FAIL: $*" >&2; exit 1; }

# Every step gets three minutes; a step that hangs fails the check instead.
k() { perl -e 'alarm shift; exec @ARGV' 180 "$JOLTA" --data-dir "$DIR" "$@" || fail "jolta $* stopped or took over three minutes"; }
q() { sqlite3 -readonly "$DIR/jolta.sqlite" "$1"; }
step() { echo "  ok  $*"; }

rm -rf "$DIR"
k account list >/dev/null
cp "$REAL/oauth-clients.env" "$DIR/" 2>/dev/null || true

for address in "$@"; do
	id=$(sqlite3 -readonly "$REAL/jolta.sqlite" "SELECT id FROM account WHERE address = '$address' COLLATE NOCASE")
	[ -n "$id" ] || fail "no account $address in the real store"
	count=$(sqlite3 -readonly "$REAL/jolta.sqlite" "SELECT count(*) FROM message WHERE account_id = $id")
	[ "$count" -le 500 ] || fail "$address has $count messages; the check only runs on test mailboxes"
	sqlite3 "$DIR/jolta.sqlite" "ATTACH 'file:$REAL/jolta.sqlite?mode=ro' AS real; INSERT INTO account SELECT * FROM real.account WHERE id = $id;"
done

# Syncs until the query gives the wanted answer, for up to a minute.
wait_for() {
	local what=$1 sql=$2 want=$3 account=$4
	for _ in $(seq 1 20); do
		k sync "$account" >/dev/null 2>&1
		[ "$(q "$sql")" = "$want" ] && return 0
		sleep 2
	done
	fail "$what (wanted $want, got $(q "$sql"))"
}

# Folders and flags of every message the check didn't send, one line each.
others() {
	q "SELECT m.remote_id || ' ' || m.flags || ' ' || coalesce((SELECT group_concat(b.remote_id) FROM message_mailbox mm JOIN mailbox b ON b.id = mm.mailbox_id WHERE mm.message_id = m.id), '')
	   FROM message m WHERE m.account_id = $1 AND m.subject NOT LIKE '%$TOKEN%' ORDER BY m.remote_id"
}

for address in "$@"; do
	id=$(q "SELECT id FROM account WHERE address = '$address' COLLATE NOCASE")
	echo "== $address (account $id)"
	k sync "$id" >/dev/null
	before=$(others "$id")
	step "first sync: $(q "SELECT count(*) FROM message WHERE account_id = $id") messages"

	subject="Jolta live check $TOKEN"
	k send "$id" --to "$address" --subject "$subject" --body "Sent by scripts/live-check.sh. It ends in Trash." >/dev/null
	in_box() {
		echo "SELECT count(*) FROM message m JOIN message_mailbox mm ON mm.message_id = m.id JOIN mailbox b ON b.id = mm.mailbox_id
		      WHERE m.account_id = $id AND b.role = '$1' AND m.subject LIKE '%$TOKEN%'"
	}
	wait_for "the message arrived in the inbox" "$(in_box inbox)" 1 "$id"
	message=$(q "SELECT m.id FROM message m JOIN message_mailbox mm ON mm.message_id = m.id JOIN mailbox b ON b.id = mm.mailbox_id
	             WHERE m.account_id = $id AND b.role = 'inbox' AND m.subject = '$subject'")
	thread=$(q "SELECT thread_id FROM message WHERE id = $message")
	[ "$(q "SELECT count(*) FROM message WHERE thread_id = $thread AND subject NOT LIKE '%$TOKEN%'")" = 0 ] || fail "the test thread holds other mail"
	step "sent to itself and received"

	flags="SELECT flags FROM message WHERE id = $message"
	k act read "$thread" >/dev/null
	wait_for "read reached the server" "SELECT ($flags) & 1" 1 "$id"
	k act unread "$thread" >/dev/null
	wait_for "unread reached the server" "SELECT ($flags) & 1" 0 "$id"
	k act flag "$thread" >/dev/null
	wait_for "flag reached the server" "SELECT ($flags) & 2" 2 "$id"
	k act unflag "$thread" >/dev/null
	wait_for "unflag reached the server" "SELECT ($flags) & 2" 0 "$id"
	step "read, unread, flag and unflag reached the server"

	k send "$id" --reply "$message" --body "A reply, also from scripts/live-check.sh." >/dev/null
	wait_for "the reply arrived in the conversation" "$(in_box inbox)" 2 "$id"
	step "reply sent and threaded"

	k act archive "$thread" --undo >/dev/null
	wait_for "archive and undo left the conversation in the inbox" "$(in_box inbox)" 2 "$id"
	step "archive, then undo, on the server"

	archive=$(q "SELECT id FROM mailbox WHERE account_id = $id AND role IN ('archive', 'all') ORDER BY role = 'archive' DESC LIMIT 1")
	k act archive "$thread" >/dev/null
	wait_for "archive reached the server" "$(in_box inbox)" 0 "$id"
	step "archived"

	k act trash "$thread" --from "$archive" >/dev/null
	wait_for "trash from the archive reached the server" "$(in_box trash)" 2 "$id"
	sent=$(q "SELECT id FROM mailbox WHERE account_id = $id AND role = 'sent'")
	if [ "$(q "$(in_box sent)")" != 0 ]; then
		k act trash "$thread" --from "$sent" >/dev/null
		wait_for "the Sent copies reached Trash" "$(in_box sent)" 0 "$id"
	fi
	step "moved to Trash, Sent copies too"

	after=$(others "$id")
	[ "$before" = "$after" ] || { diff <(echo "$before") <(echo "$after") >&2; fail "other mail changed in $address"; }
	step "every other message is exactly as before ($(echo -n "$before" | grep -c . || true) checked)"
done

rm -rf "$DIR"
echo "All checks passed. The test messages are in each account's Trash, with $TOKEN in the subject."
