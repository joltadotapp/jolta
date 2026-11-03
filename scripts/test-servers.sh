#!/usr/bin/env bash
# Starts or stops the local mail servers the sync tests use, in Docker
# (OrbStack on the Mac mini). Both listen on 127.0.0.1 only and keep no data
# between runs.
#
#   scripts/test-servers.sh up      Dovecot IMAP on 31143, Mailpit SMTP on 31025, its API on 31080
#   scripts/test-servers.sh down
#
# The tests in crates/ffi/tests/regression/sync.rs skip themselves when
# Dovecot isn't answering on 31143.

set -euo pipefail

DOVECOT_IMAGE=dovecot/dovecot:2.4.5
MAILPIT_IMAGE=axllent/mailpit:v1.33.0
HERE=$(cd "$(dirname "$0")" && pwd)

down() {
	docker rm -f jolta-test-dovecot jolta-test-mailpit >/dev/null 2>&1 || true
}

case "${1:-}" in
up)
	down
	docker run -d --name jolta-test-dovecot \
		-p 127.0.0.1:31143:31143 \
		-e USER_PASSWORD=jolta-test \
		--tmpfs /srv/vmail:uid=1000,gid=1000 \
		-v "$HERE/test-servers/dovecot.conf:/etc/dovecot/conf.d/jolta-test.conf:ro" \
		"$DOVECOT_IMAGE" >/dev/null
	docker run -d --name jolta-test-mailpit \
		-p 127.0.0.1:31025:1025 -p 127.0.0.1:31080:8025 \
		-e MP_SMTP_AUTH_ACCEPT_ANY=1 -e MP_SMTP_AUTH_ALLOW_INSECURE=1 \
		"$MAILPIT_IMAGE" >/dev/null
	for _ in $(seq 1 50); do
		if printf 'a LOGOUT\r\n' | nc -w 1 127.0.0.1 31143 2>/dev/null | grep -q '^\* OK' \
			&& curl -sf http://127.0.0.1:31080/api/v1/info >/dev/null; then
			echo "Dovecot on 127.0.0.1:31143, Mailpit on 127.0.0.1:31025 (API 31080)."
			exit 0
		fi
		sleep 0.2
	done
	echo "The test servers didn't start; see: docker logs jolta-test-dovecot" >&2
	exit 1
	;;
down)
	down
	;;
*)
	echo "usage: $0 up|down" >&2
	exit 2
	;;
esac
