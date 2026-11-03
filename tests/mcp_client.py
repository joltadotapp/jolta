"""A minimal MCP client for checks: speaks JSON-RPC lines to jolta-mcp."""
import json
import subprocess
import sys


class Client:
    def __init__(self, bridge, name="Jolta check agent"):
        self.proc = subprocess.Popen([bridge], stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
        self.next_id = 0
        self.init = self.request("initialize", {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": name, "version": "1"},
        })
        self.notify("notifications/initialized")

    def notify(self, method, params=None):
        self.proc.stdin.write(json.dumps({"jsonrpc": "2.0", "method": method, "params": params or {}}) + "\n")
        self.proc.stdin.flush()

    def request(self, method, params=None):
        self.next_id += 1
        self.proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params or {}}) + "\n")
        self.proc.stdin.flush()
        reply = json.loads(self.proc.stdout.readline())
        assert reply["id"] == self.next_id, reply
        return reply

    def call(self, tool, **args):
        r = self.request("tools/call", {"name": tool, "arguments": args})["result"]
        return r["content"][0]["text"], r["isError"]

    def close(self):
        self.proc.stdin.close()
        self.proc.wait(timeout=5)


if __name__ == "__main__":
    c = Client(sys.argv[1])
    print(json.dumps(c.init["result"]["serverInfo"]), c.init["result"]["protocolVersion"])
    print([t["name"] for t in c.request("tools/list")["result"]["tools"]])
    print(c.call("search", query="jolta send test", limit=3))
    c.close()
