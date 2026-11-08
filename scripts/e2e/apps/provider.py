# HTTPS fake provider of the MCK-19 e2e check (scripts/e2e.sh, mock mode). Stdlib only, from the ConfigMap
# provider-code into the pinned python image. 200 with the body "ok" for any GET, over the certificate in /certs.
# One JSON access line per request: method, path, status, peer. Never headers or bodies, so the logs of a
# test can be checked for canaries.
import http.server
import json
import ssl

CONTEXT = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
CONTEXT.load_cert_chain("/certs/tls.crt", "/certs/tls.key")


class Handler(http.server.BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def setup(self):
        self.request = CONTEXT.wrap_socket(self.request, server_side=True)
        super().setup()

    def do_GET(self):
        body = b"ok\n"
        print(json.dumps({"app": "provider", "method": self.command, "path": self.path, "status": 200,
                          "peer": self.client_address[0]}), flush=True)
        self.send_response(200)
        self.send_header("Content-Type", "text/plain")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, *args):
        pass


http.server.ThreadingHTTPServer(("0.0.0.0", 8443), Handler).serve_forever()
