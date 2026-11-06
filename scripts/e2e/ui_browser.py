# Loads one page in a headless Chromium through the DevTools protocol and prints what it saw as JSON (UI-12).
# Stdlib only (a minimal WebSocket client), run by scripts/e2e.sh with python3 -I. Usage:
#   ui_browser.py <devtools port> <url> [--wait TEXT]... [--expand] [--interceptor]
# It waits until the page text holds every TEXT (up to 45 s), with --expand opens every operation and walks
# every example selector, settles for 3 s so that a stray request would show, then prints:
#   text       the page text (innerText), with --expand also what each selector option showed
#   dom        the DOM (outerHTML)
#   buttons    the text of every button
#   methods    the text of every operation method label
#   xss_imgs   the number of img elements whose src holds "ui-xss"
#   requests   the URL of every request the page made
#   interceptor with --interceptor, the outcome of window.mimicRequestInterceptor on its URLs: for each, whether
#              it threw (a refusal) or returned the request
#   violations CSP violations (console, log and audit issues, requests blocked by the policy)
#   errors     other console errors and uncaught exceptions
import base64
import json
import os
import socket
import struct
import sys
import time
import urllib.request


class Cdp:
    def __init__(self, ws_url):
        authority, path = ws_url.split("//", 1)[1].split("/", 1)
        host, port = authority.rsplit(":", 1)
        self.sock = socket.create_connection((host, int(port)), timeout=60)
        key = base64.b64encode(os.urandom(16)).decode()
        self.sock.sendall(
            f"GET /{path} HTTP/1.1\r\nHost: {authority}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n"
            f"Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n".encode()
        )
        head = b""
        while b"\r\n\r\n" not in head:
            chunk = self.sock.recv(1)
            if not chunk:
                raise SystemExit("devtools: the handshake was cut")
            head += chunk
        if b" 101 " not in head.split(b"\r\n", 1)[0]:
            raise SystemExit(f"devtools: handshake refused: {head!r}")
        self.buf = bytearray()
        self.partial = None
        self.next_id = 0
        self.events = []

    def _fill(self, timeout):
        """Reads what the socket has into the buffer; False when nothing came within the timeout."""
        self.sock.settimeout(timeout)
        try:
            chunk = self.sock.recv(65536)
        except (socket.timeout, TimeoutError):
            return False
        if not chunk:
            raise SystemExit("devtools: connection closed")
        self.buf += chunk
        return True

    def _parse(self):
        """One complete message out of the buffer, or None while a frame is incomplete. A frame is consumed
        only once it is whole, so a timeout between two reads never splits one. A frame that a server cannot
        send (masked, reserved bits, unknown opcode, a stray continuation) means the stream is out of step."""
        while True:
            if len(self.buf) < 2:
                return None
            b0, b1 = self.buf[0], self.buf[1]
            if b0 & 0x70 or b1 & 0x80:
                raise SystemExit("devtools: the WebSocket stream is out of step (reserved bits or a masked frame)")
            opcode, length, offset = b0 & 0x0F, b1 & 0x7F, 2
            if length == 126:
                if len(self.buf) < 4:
                    return None
                (length,) = struct.unpack(">H", self.buf[2:4])
                offset = 4
            elif length == 127:
                if len(self.buf) < 10:
                    return None
                (length,) = struct.unpack(">Q", self.buf[2:10])
                offset = 10
            if len(self.buf) < offset + length:
                return None
            payload = bytes(self.buf[offset : offset + length])
            del self.buf[: offset + length]
            if opcode == 8:
                raise SystemExit("devtools: connection closed by the browser")
            if opcode in (9, 10):
                continue
            if opcode in (1, 2):
                if self.partial is not None:
                    raise SystemExit("devtools: the WebSocket stream is out of step (data frame inside a message)")
                self.partial = b""
            elif opcode == 0:
                if self.partial is None:
                    raise SystemExit("devtools: the WebSocket stream is out of step (stray continuation)")
            else:
                raise SystemExit(f"devtools: the WebSocket stream is out of step (opcode {opcode})")
            self.partial += payload
            if b0 & 0x80:
                message, self.partial = self.partial, None
                return message

    def _message(self, timeout):
        """The next message as parsed JSON, or None when none is complete within the timeout."""
        end = time.time() + timeout
        while True:
            raw = self._parse()
            if raw is not None:
                try:
                    return json.loads(raw)
                except ValueError:
                    raise SystemExit("devtools: a message is not JSON: the stream is out of step")
            left = end - time.time()
            if left <= 0 or not self._fill(left):
                return None

    def _send(self, text):
        payload = text.encode()
        mask = os.urandom(4)
        n = len(payload)
        head = bytes([0x81])
        if n < 126:
            head += bytes([0x80 | n])
        elif n < 65536:
            head += bytes([0x80 | 126]) + struct.pack(">H", n)
        else:
            head += bytes([0x80 | 127]) + struct.pack(">Q", n)
        self.sock.sendall(head + mask + bytes(b ^ mask[i % 4] for i, b in enumerate(payload)))

    def call(self, method, **params):
        self.next_id += 1
        want = self.next_id
        self._send(json.dumps({"id": want, "method": method, "params": params}))
        while True:
            message = self._message(60)
            if message is None:
                raise SystemExit(f"devtools: {method}: no answer within 60 s")
            if message.get("id") == want:
                if "error" in message:
                    raise SystemExit(f"devtools: {method}: {message['error']}")
                return message["result"]
            if "method" in message:
                self.events.append(message)

    def drain(self, seconds):
        end = time.time() + seconds
        while time.time() < end:
            message = self._message(max(end - time.time(), 0.01))
            if message is not None and "method" in message:
                self.events.append(message)

    def eval(self, expression):
        result = self.call("Runtime.evaluate", expression=expression, returnByValue=True, awaitPromise=True)
        if "exceptionDetails" in result:
            raise SystemExit(f"devtools: evaluating the page script failed: {result['exceptionDetails']}")
        return result["result"].get("value")


EXPAND = """(async () => {
  const texts = [];
  const pause = (ms) => new Promise((r) => setTimeout(r, ms));
  for (const block of document.querySelectorAll('.opblock')) {
    if (!block.classList.contains('is-open')) {
      const control = block.querySelector('.opblock-summary-control') || block.querySelector('.opblock-summary');
      if (control) control.click();
    }
  }
  await pause(1000);
  const setter = Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value').set;
  for (const select of document.querySelectorAll('.opblock-body select')) {
    for (const option of Array.from(select.options)) {
      setter.call(select, option.value);
      select.dispatchEvent(new Event('change', { bubbles: true }));
      await pause(300);
      texts.push(document.body.innerText);
    }
  }
  for (const tab of document.querySelectorAll('.opblock-body .tab-header .tablinks')) {
    tab.click();
    await pause(300);
    texts.push(document.body.innerText);
  }
  return texts;
})()"""

COLLECT = """JSON.stringify({
  text: document.body.innerText,
  dom: document.documentElement.outerHTML,
  buttons: Array.from(document.querySelectorAll('button')).map((b) => b.innerText || b.textContent),
  methods: Array.from(document.querySelectorAll('.opblock-summary-method')).map((m) => m.textContent),
  xss_imgs: document.querySelectorAll('img[src*="ui-xss"]').length,
})"""


INTERCEPTOR = """(() => {
  const o = window.location.origin;
  const urls = {
    cross_origin: "http://localhost:18090/ref",
    cross_origin_workloads: "http://other.example/v1/workloads/x",
    same_origin_outside: o + "/ui/assets/mimic.js",
    malformed: "http://[",
    prefix_trick: o + "/v1/workloadsX",
    ui_path: o + "/ui/x",
    same_origin_workloads: o + "/v1/workloads/disc-22/users/openapi/inbound-8080",
  };
  const out = {};
  for (const [name, url] of Object.entries(urls)) {
    const request = { url: url, headers: {} };
    try {
      const result = window.mimicRequestInterceptor(request);
      out[name] = { threw: false, passed: result === request };
    } catch (error) {
      out[name] = { threw: true, passed: false };
    }
  }
  return out;
})()"""


def is_csp(text):
    return "Content Security Policy" in text or "Refused to" in text


def main():
    port, url, rest = sys.argv[1], sys.argv[2], sys.argv[3:]
    waits = [rest[i + 1] for i, a in enumerate(rest) if a == "--wait"]
    expand = "--expand" in rest
    request = urllib.request.Request(f"http://127.0.0.1:{port}/json/new?about:blank", method="PUT")
    with urllib.request.urlopen(request, timeout=30) as r:
        target = json.load(r)
    cdp = Cdp(target["webSocketDebuggerUrl"])
    for domain in ("Network", "Log", "Runtime", "Audits", "Page"):
        cdp.call(f"{domain}.enable")
    cdp.call("Page.navigate", url=url)
    end = time.time() + 45
    text = ""
    while time.time() < end:
        cdp.drain(0.5)
        text = cdp.eval("document.body ? document.body.innerText : ''") or ""
        if all(w in text for w in waits):
            break
    else:
        print(f"ui_browser: {url}: the page never showed {waits}; it shows {text[:500]!r}", file=sys.stderr)
        sys.exit(1)
    extra = cdp.eval(EXPAND) if expand else []
    interceptor = cdp.eval(INTERCEPTOR) if "--interceptor" in rest else None
    if interceptor is not None:
        wrong = [n for n, r in interceptor.items() if r["threw"] != (n != "same_origin_workloads")]
        if wrong:
            print(f"ui_browser: the request interceptor misjudged {wrong}: {interceptor}", file=sys.stderr)
            sys.exit(1)
    cdp.drain(3)
    seen = json.loads(cdp.eval(COLLECT))
    seen["text"] = "\n".join([seen["text"], *extra])
    requests, violations, errors = [], [], []
    for event in cdp.events:
        p, name = event.get("params", {}), event["method"]
        if name == "Network.requestWillBeSent":
            requests.append(p["request"]["url"])
        elif name == "Network.loadingFailed" and p.get("blockedReason") == "csp":
            violations.append(f"blocked by csp: {p.get('requestId')}")
        elif name == "Audits.issueAdded" and p["issue"]["code"] == "ContentSecurityPolicyIssue":
            violations.append(json.dumps(p["issue"]["details"]))
        elif name == "Log.entryAdded":
            entry = p["entry"]
            if is_csp(entry.get("text", "")):
                violations.append(entry["text"])
            elif entry.get("level") == "error":
                errors.append(f"{entry.get('url', '')}: {entry.get('text', '')}")
        elif name == "Runtime.consoleAPICalled":
            line = " ".join(str(a.get("value", a.get("description", ""))) for a in p["args"])
            if is_csp(line):
                violations.append(line)
            elif p["type"] == "error":
                errors.append(line)
        elif name == "Runtime.exceptionThrown":
            errors.append(json.dumps(p["exceptionDetails"].get("exception", {}).get("description", "")))
    seen["interceptor"] = interceptor
    seen.update(requests=requests, violations=violations, errors=errors)
    print(json.dumps(seen))


main()
