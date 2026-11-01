# E2E test service (scripts/e2e/apps). Stdlib only, mounted from the ConfigMap apps-code into a pinned
# python:3.13-slim image, so no image is built for the test apps.
#
#   APP_NAME  name in the access lines (default "app")
#   PORT      listening port (default 8080)
#   UPSTREAM  optional base URL (service B). With it, /front/<path> is forwarded to UPSTREAM/<path> with
#             the Authorization, X-Tenant-Key and Content-Type headers, and the upstream status and body
#             are returned. Without it, /users/<id> answers JSON (GET and PUT) and any other path 404s.
#
# Each request writes one JSON access line to stdout: app, method, path, status, peer. Never headers or
# bodies, so the logs of a test can be checked for canaries.
import http.server
import json
import os
import urllib.error
import urllib.request

NAME = os.environ.get("APP_NAME", "app")
UPSTREAM = os.environ.get("UPSTREAM", "").rstrip("/")
PORT = int(os.environ.get("PORT", "8080"))
FORWARDED = ("Authorization", "X-Tenant-Key", "Content-Type")


def access(method, path, status, peer):
    print(json.dumps({"app": NAME, "method": method, "path": path, "status": status, "peer": peer}), flush=True)


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *args):
        pass  # the access line is written by access()

    def _reply(self, status, body, content_type="application/json"):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)
        access(self.command, self.path, status, self.client_address[0])

    def _json(self, status, obj):
        self._reply(status, json.dumps(obj).encode())

    def _body(self):
        length = int(self.headers.get("Content-Length") or 0)
        return self.rfile.read(length) if length else b""

    def do_GET(self):
        self._route(b"")

    def do_PUT(self):
        self._route(self._body())

    def do_POST(self):
        self._route(self._body())

    def _route(self, body):
        if UPSTREAM and self.path.startswith("/front/"):
            return self._forward(body)
        parts = self.path.split("?", 1)[0].split("/")
        if not UPSTREAM and len(parts) == 3 and parts[1] == "users" and parts[2]:
            if self.command == "PUT":
                payload = json.loads(body or b"{}")
                return self._json(200, {"id": parts[2], "updated": True, "name": payload.get("name")})
            return self._json(200, {"id": parts[2], "name": "user " + parts[2], "active": True, "score": 1.5})
        self._json(404, {"error": "not found"})

    def _forward(self, body):
        target = UPSTREAM + self.path[len("/front"):]
        headers = {name: self.headers[name] for name in FORWARDED if self.headers.get(name)}
        request = urllib.request.Request(
            target, data=None if self.command == "GET" else body, method=self.command, headers=headers
        )
        try:
            with urllib.request.urlopen(request, timeout=10) as upstream:
                status, data = upstream.status, upstream.read()
                content_type = upstream.headers.get("Content-Type", "application/json")
        except urllib.error.HTTPError as error:
            status, data = error.code, error.read()
            content_type = error.headers.get("Content-Type", "application/json")
        except urllib.error.URLError:
            status, data, content_type = 502, b'{"error": "upstream unreachable"}', "application/json"
        self._reply(status, data, content_type)


if __name__ == "__main__":
    http.server.ThreadingHTTPServer(("0.0.0.0", PORT), Handler).serve_forever()
