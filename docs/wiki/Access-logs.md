# Access logs

Audience: ops and developers reading the traffic of a pod.

Each sidecar writes one JSON line per HTTP exchange and one per opaque or TLS connection, when it ends,
on its stdout (`target` `access`, level `INFO`). Read them with `kubectl logs`:

```bash
kubectl -n demo logs deploy/front -c mimic-sidecar | jq -c 'select(.target == "access")'
```

A pod with several replicas has one stream per pod: use `kubectl logs <pod>` for each, or `-l app=front`
(one stream per matching pod, each line still JSON). Lines are dropped, not blocked, when stdout cannot keep
up: `mimic_log_lines_dropped_total` counts them.

## The two messages

| `message` | Written for | Example fields |
|---|---|---|
| `http exchange` | one HTTP/1 or HTTP/2 request and its response, inbound or outbound | `method`, `path`, `status`, `duration_ms` |
| `connection` | one opaque TCP connection or one TLS connection that is not decrypted | `sni`, `interception`, `duration_ms`, `bytes_in` |

A line appears when the exchange or the connection ends, so a long-lived request (a stream, a WebSocket
tunnel) is logged late, with its full duration. Each line also has `timestamp` (UTC), `level` (`INFO`),
`target` (`access`) and `message`.

## Sample lines

> **Pending.** The sample lines of this section are to be captured from a kind run of the chart
> (`scripts/e2e.sh`, check `ui_14`) and pasted here as they were logged, one inbound and one outbound
> `http exchange` and one `connection`. They are not written by hand.

## Fields

Only the fields below are logged. Header values, cookies, query strings and bodies are never logged,
decrypted or not. A field that has no value is absent from the line, not `null`.

| Field | Meaning |
|---|---|
| `direction` | `inbound` (a request to the pod's app) or `outbound` (a request the app makes). |
| `protocol` | HTTP: `http1` or `h2`. Connection: `tls` or `tcp`. |
| `dst` | The original destination, `ip:port`, as the pod saw it before the redirection. |
| `authority` | The target authority, normalised: the absolute-form target of an HTTP/1 request, else the `Host` header; for HTTP/2 `:authority`, else `Host`. The default port is 80, or 443 on an intercepted TLS connection. Absent `Host` gives `dst`. Use it to group by service. Cut to 1024 bytes. |
| `sni` | Connection only, TLS only, when the client sent one. |
| `method` | The HTTP method, at most 64 bytes. |
| `path` | The path without the query string, at most 1024 bytes. Values the redaction detectors recognise are logged as their placeholder (for example `mimic-jwt`, `mimic-card`). |
| `status` | The response status. Absent when no response was sent. |
| `grpc_status` | HTTP/2 only, from the trailers, when present. `0` is OK. |
| `upgrade` | `true` when the exchange became a tunnel (WebSocket, `CONNECT`). Always present on an HTTP exchange. |
| `tls` | `true` when the exchange was decrypted by TLS interception. Absent otherwise. |
| `interception` | Connection only, outbound TLS only: why the connection was not intercepted (a decision other than `intercepted`, which writes exchange lines instead). |
| `result` | How it ended. Exchange: `ok`, `upstream_reset` (the destination reset it), `client_abort` (the app went away), `unframed` (HTTP/1 the codec cannot frame; the rest of the connection is a tunnel), `rejected` (HTTP/2 connection error), `shutdown` (closed at the end of the grace). Connection: `ok`, `upstream_reset`, `client_abort`, `shutdown`, `mock_closed` (closed by the mock). |
| `mock` | Outbound, in mock mode only: `exact`, `endpoint`, `fallback` or `miss`: the tier of the mock that answered. |
| `mock_miss` | With `mock` `miss`: why the mock had no answer. |
| `load_run` | The uid of the load test run that sent a request carrying the load marker. Absent otherwise. |
| `duration_ms` | Time until the response end (exchange) or the connection end. Absent for `unframed` and `rejected`. |
| `head_ms` | Time until the final response head (not a `1xx`). Absent for `unframed` and `rejected`. |
| `bytes_in` | Request body bytes (exchange), or bytes from the client to the destination (connection). |
| `bytes_out` | Response body bytes (exchange), or bytes from the destination to the client (connection). |

The sidecar's metrics count the same exchanges (`mimic_http_requests_total`, `mimic_http_request_duration_seconds`,
see [Operations](Operations#sidecar)).

## Recipes

Each recipe takes the lines of one pod (or `kubectl logs deploy/<name> -c mimic-sidecar`) and filters them
with `jq`. Replace `demo` and `front`.

Exchanges only, from here on:

```bash
LOGS="kubectl -n demo logs deploy/front -c mimic-sidecar"
$LOGS | jq -c 'select(.message == "http exchange")'
```

By status class, per direction (`status` is absent when no response was sent, so such lines are left out):

```bash
$LOGS | jq -r 'select(.message == "http exchange" and .status != null)
  | "\(.direction) \(.status / 100 | floor)xx"' | sort | uniq -c
```

Only the server errors, with the authority and the path:

```bash
$LOGS | jq -c 'select(.message == "http exchange" and (.status // 0) >= 500) | {authority, method, path, status, result}'
```

By authority, most called first:

```bash
$LOGS | jq -r 'select(.message == "http exchange") | "\(.direction) \(.authority)"' | sort | uniq -c | sort -rn
```

Slow calls, over one second, with the time to the response head:

```bash
$LOGS | jq -c 'select(.message == "http exchange" and (.duration_ms // 0) > 1000) | {authority, path, status, duration_ms, head_ms}'
```

Mock results (mock mode, outbound), and the misses with their reason:

```bash
$LOGS | jq -c 'select(.mock != null) | {authority, path, mock, mock_miss, status}'
```

Load-marked requests (a load test run sent them; the run is named by its uid):

```bash
$LOGS | jq -c 'select(.load_run != null) | {load_run, direction, authority, path, status, duration_ms}'
```

Connections that were not decrypted, with why and how they ended:

```bash
$LOGS | jq -c 'select(.message == "connection") | {direction, dst, sni, interception, result, duration_ms, bytes_out}'
```

## What the lines do not tell

- Bodies, headers and queries: never, so a payload problem needs the app's own logs.
- Requests that the codec or the HTTP/2 stack refused before a stream existed: no line (see the accepted limits in
  the spec, §4.1).
- HTTPS passed through without interception: only a `connection` line (SNI, bytes, duration).

See [Troubleshooting](Troubleshooting) for symptoms and [Operations](Operations) for the other log targets.
