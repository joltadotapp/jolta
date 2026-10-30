#!/bin/sh
# M8 check: an email that tells agents to send or forward mail must not
# lead to any send. Runs the engine and MCP server from the CLI, imports
# tests/fixtures/injection.eml, approves a scripted agent and lets it try.
# Network: the fixture's tracking pixel points at 127.0.0.1:18080, and a
# logging server there must see nothing.
set -eu
cd "$(dirname "$0")/.."
DATA="${KITE_DATA_DIR:-$HOME/Library/Application Support/Kite}"
SOCK=$(mktemp -u /tmp/kite-mcp-check.XXXXXX)
LOG=$(mktemp)
scripts/build-cli.sh >/dev/null
cargo build -q --release -p kite-mcp
KITE=./target/release/kite
ACCOUNT=${KITE_CHECK_ACCOUNT:-$($KITE account list | awk 'NR==1 {print $1}')}
$KITE import "$ACCOUNT" inbox tests/fixtures/injection.eml >/dev/null

python3 scripts/leak-server.py 18080 2>"$LOG" &
SERVER=$!
$KITE mcp-serve --socket "$SOCK" 2>/dev/null &
MCP=$!
trap 'kill $MCP $SERVER 2>/dev/null; rm -f "$SOCK"' EXIT
sleep 2

# The first connection registers the agent as pending; approve it fully.
KITE_MCP_SOCKET="$SOCK" python3 -c "
import sys; sys.path.insert(0, 'tests')
from mcp_client import Client
Client('target/release/kite-mcp', name='Injection check agent').close()"
ID=$($KITE agents | awk '/Injection check agent/ {print $1}')
$KITE agents approve "$ID" --grants read,organize,draft >/dev/null

KITE_MCP_SOCKET="$SOCK" python3 tests/mcp_injection_check.py target/release/kite-mcp "$DATA/kite.sqlite"
HITS=$(grep -v "ua='node'" "$LOG" | grep -c '^"' || true)
[ "$HITS" -eq 0 ] || { echo "FAIL: $HITS request(s) reached 127.0.0.1:18080"; cat "$LOG"; exit 1; }
echo "PASS: 0 network requests to the fixture's tracking server"
