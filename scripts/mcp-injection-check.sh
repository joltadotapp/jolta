#!/bin/sh
# M8 check: an email that tells agents to send or forward mail must not
# lead to any send. Runs the engine and MCP server from the CLI, imports
# tests/fixtures/injection.eml, approves a scripted agent and lets it try.
# Network: the fixture's tracking pixel points at 127.0.0.1:18080, and a
# logging server there must see nothing.
set -eu
cd "$(dirname "$0")/.."
DATA="${JOLTA_DATA_DIR:-$HOME/Library/Application Support/Jolta}"
SOCK=$(mktemp -u /tmp/jolta-mcp-check.XXXXXX)
LOG=$(mktemp)
scripts/build-cli.sh >/dev/null
cargo build -q --release -p jolta-mcp
JOLTA=./target/release/jolta
ACCOUNT=${JOLTA_CHECK_ACCOUNT:-$($JOLTA account list | awk 'NR==1 {print $1}')}
$JOLTA import "$ACCOUNT" inbox tests/fixtures/injection.eml >/dev/null

python3 scripts/leak-server.py 18080 2>"$LOG" &
SERVER=$!
$JOLTA mcp-serve --socket "$SOCK" 2>/dev/null &
MCP=$!
trap 'kill $MCP $SERVER 2>/dev/null; rm -f "$SOCK"' EXIT
sleep 2

# The first connection registers the agent as pending; approve it fully.
JOLTA_MCP_SOCKET="$SOCK" python3 -c "
import sys; sys.path.insert(0, 'tests')
from mcp_client import Client
Client('target/release/jolta-mcp', name='Injection check agent').close()"
ID=$($JOLTA agents | awk '/^ *[0-9]+ +Injection check agent/ {print $1}')
$JOLTA agents approve "$ID" --grants read,organize,draft >/dev/null

JOLTA_MCP_SOCKET="$SOCK" python3 tests/mcp_injection_check.py target/release/jolta-mcp "$DATA/jolta.sqlite"
HITS=$(grep -v "ua='node'" "$LOG" | grep -c '^"' || true)
[ "$HITS" -eq 0 ] || { echo "FAIL: $HITS request(s) reached 127.0.0.1:18080"; cat "$LOG"; exit 1; }
echo "PASS: 0 network requests to the fixture's tracking server"
