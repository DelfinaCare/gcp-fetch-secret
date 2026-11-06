"""Tiny stand-in for the Secret Manager REST API, used by the CI container test.

Serves GET /v1/projects/P/secrets/S/versions/V[:access]. Secrets and their
versions come from a JSON file (re-read on every request, so the test can
rotate a secret while secretsfetch is running):

    {"projects/p/secrets/db": {"version": 1, "data": "hunter2"}}
"""

import base64
import json
import sys
from urllib.parse import urlsplit
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


class Handler(BaseHTTPRequestHandler):
    def do_GET(self):
        path = urlsplit(self.path).path.removeprefix("/v1/")
        access = path.endswith(":access")
        path = path.removesuffix(":access")
        secret, _, _ = path.rpartition("/versions/")
        with open(sys.argv[2]) as f:
            secrets = json.load(f)
        if secret not in secrets:
            self.reply(404, {"error": {"code": 404, "message": f"{secret} not found", "status": "NOT_FOUND"}})
            return
        entry = secrets[secret]
        body = {"name": f"{secret}/versions/{entry['version']}"}
        if access:
            body["payload"] = {"data": base64.b64encode(entry["data"].encode()).decode()}
        self.reply(200, body)

    def reply(self, status, body):
        data = json.dumps(body).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


if __name__ == "__main__":
    ThreadingHTTPServer(("127.0.0.1", int(sys.argv[1])), Handler).serve_forever()
