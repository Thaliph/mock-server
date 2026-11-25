# Open-model HTTP client of the e2e scripts (scripts/e2e-autoscaling.sh): one thread per request, started on a fixed clock of
# <rate> requests per second whatever the answers do, each on a new connection. Stdlib only. Never prints a header or a body.
#
#   python3 open_client.py <url> <rate>
import sys
import threading
import time
import urllib.request

url, rate = sys.argv[1], float(sys.argv[2])


def one():
    try:
        urllib.request.urlopen(url, timeout=5).read()
    except Exception:
        pass


start, n = time.monotonic(), 0
while True:
    n += 1
    threading.Thread(target=one, daemon=True).start()
    time.sleep(max(0.0, start + n / rate - time.monotonic()))
