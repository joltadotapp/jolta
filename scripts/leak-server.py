"""Logs every request to stderr, for scripts/leak-check.sh."""
import http.server
import sys


class Handler(http.server.BaseHTTPRequestHandler):
    def do_GET(self):
        ua = self.headers.get("User-Agent")
        sys.stderr.write(f'"{self.command} {self.path}" ua={ua!r}\n')
        sys.stderr.flush()
        self.send_response(404)
        self.end_headers()

    do_POST = do_HEAD = do_GET

    def log_message(self, *args):
        pass


http.server.HTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
