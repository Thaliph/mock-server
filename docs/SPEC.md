# Mimic — specification

> Status: **v2.0-draft**, rewritten from scratch on 2026-10-31. The v0.x codebase is kept under
> the git tag `legacy-v0.37` for reference only. This file is the source of truth: any change of
> behaviour is written here first, then tested, then implemented.

## 1. Purpose

Mimic is a Kubernetes sidecar and its control plane. It serves two goals for an infrastructure
SRE, without any change to application code:

1. **Document** the HTTP APIs that run on a cluster: every injected deployment gets an
   OpenAPI 3.1 description of the endpoints it exposes (ingress) and of the external APIs it
   calls (egress), built from real traffic.
2. **Load-test one workload in isolation**: in *mock* mode, the deployment's egress calls are
   answered by the sidecar from recorded responses, with recorded latencies, so a payment
   service can be pushed to saturation without calling its provider (e.g. Adyen) and without
   being rate-limited. From F11 mimic also generates that load: a `MimicLoadTest` replays the
   deployment's discovered ingress against its pods while it is in mock, and reports latency,
   errors and throughput per endpoint (§7.8).

### 1.1 In scope (v1)

- HTTP/1.1, HTTP/2 and unary gRPC, cleartext or TLS, inbound and outbound.
- Discovery mode (record) and mock mode, switched per deployment.
- Access logs, detailed component logs, Prometheus RED metrics for ingress and egress.
- A read-only UI per deployment (OpenAPI + Swagger UI).
- (F11) Load tests of a deployment in mock mode, built from its discovered ingress (§7.8).

### 1.2 Out of scope (v1)

- Traces (no OpenTelemetry spans).
- Traffic management (retries, routing, fault injection beyond mock latency) and mesh security
  (mTLS between workloads, authorization policies): candidate v2, "mesh" features.
- Recording or mocking WebSocket, SSE, gRPC streaming or non-HTTP TCP: passed through, logged
  and measured only.
- Fail-open when the sidecar is down (would need a privileged node agent): v2.
- Kubernetes older than 1.33 (native sidecars are required).

## 2. Architecture

```
            ┌──────────────────────── pod (injected) ────────────────────────┐
 client ──▶ │ iptables ─▶ mimic-sidecar ─▶ app ─▶ iptables ─▶ mimic-sidecar ─┼─▶ provider
            │  (inbound)   discovery: record + forward      (outbound)        │   (or mock)
            │              local cache (endpoints, egress samples)            │
            └───────────────────────────────┬────────────────────────────────┘
                                            │ HTTP/JSON (push recordings, pull mode + egress)
                                  ┌─────────▼──────────┐       ┌────────┐
                                  │  mimic-controller  │──────▶│ Valkey │
                                  │ webhook · API · UI │       └────────┘
                                  └────────────────────┘
```

| Component | Kind | Role |
|---|---|---|
| `mimic-init` | first init container, exits | Installs the iptables redirection in the pod network namespace (§4). Runs as UID 0 with only `NET_ADMIN` and `NET_RAW`, own image `ghcr.io/thaliph/mimic-init`. |
| `mimic-sidecar` | native sidecar (second init container, `restartPolicy: Always`) | Transparent proxy for inbound and outbound traffic; records, measures, mocks. UID 1337, no capability. |
| `mimic-trust` | init container per app image, exits (from F3) | Runs mimic's static `mimic-trust` binary inside an app image to build that image's trust bundle plus the deployment's root CA (§5, §7.1). UID 1337, no capability, no network. |
| `mimic-load` | Job per load test, in the namespace of the deployment under test (F11) | Sends the planned HTTP load to the deployment's pods while they are in mock, posts its results to the controller (§7.8). UID 65532, no capability, no Kubernetes API token. |
| `mimic-controller` | Deployment in `mimic-system` | Mutating webhook (injection), `MimicWorkload` reconciler, sidecar API (§7.2), UI. Stateless: all state is in Valkey, except the TLS seed kept in a Secret (§5). |
| Valkey | StatefulSet in `mimic-system` | Shared store: merged endpoints and egress recordings per deployment. AOF persistence on a PVC. |

Crates: `mimic-core` (pure logic: no tokio, no I/O, no clock), `mimic-runtime` (shared process
plumbing: configuration, logs, metrics, health, shutdown), `mimic-sidecar`, `mimic-controller`,
from F1 `mimic-init`, from F3 `mimic-trust` and from F11 `mimic-load`. `mimic-core` may depend on pure data crates
(`serde`, `serde_json`, `k8s-openapi`, `rustls-pki-types`).

## 3. Modes

| Mode | Inbound | Outbound |
|---|---|---|
| `discovery` (default) | forwarded to the app, recorded | forwarded to the destination, recorded |
| `mock` | forwarded to the app, recorded | HTTP answered from recordings, never forwarded nor recorded; non-HTTP cleartext TCP and `spec.mock.passthrough` destinations forwarded and recorded; other TLS closed (§8) |

The mode is set per deployment on its `MimicWorkload` (`spec.mode`); when several
`MimicWorkload`s name the deployment, it is `mock` as soon as one of them says `mock` (§7).
Every pod of the deployment follows it, including pods created later, without a restart
(F10): the webhook gives a new pod the mode of the moment in `MIMIC_MODE`, and every sidecar
learns changes from its egress pull (§7.7), which carries the mode with the snapshot of the
deployment's aggregated egress. A change therefore reaches every pod within one
`MIMIC_EGRESS_PULL_INTERVAL` (plus the pull's own retry delay while the controller is
unreachable); `status.mode` of the `MimicWorkload` counts the pods that follow it (§7.4).
Entering mock: the pull answer that carries `mock` also carries the current egress snapshot
(or confirms it with `304`), so the sidecar switches with it; requests started after the
switch are answered from it. Leaving mock: the sidecar switches back to discovery at the
answer that carries `discovery`; mocked connections close once their answers are sent, new
connections reach the real destinations and are recorded again (§8 "Switching"). A load test
(F11) runs only while the deployment is in mock and stops when it leaves it (§7.8). Until a
sidecar's first pull answer it follows the mode it last saved in its pod (a restarted sidecar
container), else `MIMIC_MODE` (§8 "Mode state"); when the controller cannot be reached, it
keeps the mode it has.

## 4. Traffic capture

- Transparent redirection with iptables (`nat` table, `iptables-nft` backend) set by
  `mimic-init` through `iptables-restore` and `ip6tables-restore`, for IPv4 and IPv6:
  - inbound: TCP whose destination is a local address of the pod is redirected to the sidecar
    inbound port **15006**, except the excluded inbound ports;
  - outbound: TCP leaving the pod is redirected to the sidecar outbound port **15001**, except
    traffic from the sidecar UID **1337** and traffic on the loopback interface;
  - `filter` table: outbound UDP to port 443 is rejected (ICMP port unreachable) so that HTTP/3
    clients fall back to TCP. Other UDP (DNS included) is untouched.
  - IPv6 rules are installed only when the pod has a non-loopback IPv6 address (`::1` alone
    means no IPv6). Running `mimic-init` again, also after a partial failure, leaves exactly
    one jump from each built-in chain (idempotent): `mimic-init` owns the built-in chains it
    hooks (`nat` `PREROUTING` and `OUTPUT`, `filter` `OUTPUT`) in the pod's network namespace
    and flushes them before adding its jump. It runs first in a fresh namespace; another
    traffic-capturing tool in the same pod is not supported (§11.1).
- **HTTP probes go through the sidecar** (as Istio does). The webhook rewrites every `httpGet`
  liveness, readiness and startup probe of the app containers to `httpGet` on the sidecar admin
  port 15100, path `/app-health/<container>/<livez|readyz|startupz>`, scheme HTTP, and keeps
  the probe's timings and thresholds. It gives the original probes to the sidecar in
  `MIMIC_APP_PROBES`. The sidecar sends the original request (scheme, host, defaulting to the
  pod IP, port with named ports resolved, path, headers), from UID 1337 so that it is not
  captured. It does not verify TLS and does not follow redirects, like the kubelet. It answers
  with the app's status code, or 503 when the app cannot be reached within the probe's
  `timeoutSeconds`. Only the configured probes can be called: any other `/app-health/` path
  gets 404. Probe requests are not recorded.
- **`tcpSocket` and `grpc` probes go through the sidecar too.** The webhook rewrites them like
  `httpGet` probes (same path, port, scheme, timings and thresholds), so their ports stay
  intercepted. For a `tcpSocket` probe the sidecar opens a TCP connection to the probe's host
  (default the pod IP) and port within `timeoutSeconds`, then closes it: 200 if it connects,
  503 otherwise. For a `grpc` probe it calls `grpc.health.v1.Health/Check` in cleartext HTTP/2
  on the pod IP and the probe's port with the probe's `service` (empty by default) within
  `timeoutSeconds`: 200 only if the call succeeds with status `SERVING`, 503 otherwise. A
  `tcpSocket` probe whose named port does not resolve is left unchanged. The excluded inbound
  ports are only the sidecar admin port 15100.
- When the pod has no IPv6 (`/proc/net/if_inet6` absent or empty), the IPv6 rules and listeners
  are skipped with an INFO log; any other failure is fatal.
- The sidecar listens on `0.0.0.0:15006` and `[::]:15006` (inbound, IPv6 socket `V6ONLY`) and
  on loopback only for outbound (`127.0.0.1:15001`, `[::1]:15001`), reads the original
  destination (`SO_ORIGINAL_DST`) and connects to it from UID 1337. A connection whose original destination is missing or is a
  sidecar listener is closed (loop protection). The sidecar is ready only once its listeners are
  bound, and the app containers start only after that (native sidecar startup probe).
- Protocol detection per connection: TLS (ClientHello), HTTP/1.x, HTTP/2 prior knowledge;
  anything else is an opaque TCP tunnel (logged and measured). F1 provides the opaque tunnel
  for everything; detection and the HTTP-aware proxy come in F2 (§4.1).
- When the sidecar is down, redirected connections are refused: capture is **fail-closed**
  (fail-open is v2, §1.2).

### 4.1 Proxy

The sidecar is a pass-through proxy: what the client and the destination see is what they
would see without mimic, except for the limits listed at the end of this section.

- **Detection.** The sidecar connects to the original destination first (as in F1), then waits
  until one side sends bytes. If the destination speaks first (server-first protocols: SMTP,
  MySQL, SSH), or the client's first bytes are neither a TLS ClientHello, an HTTP/1.x request
  line nor the HTTP/2 preface, the connection is an opaque tunnel. Detection has no timeout,
  reads at most 16 KiB, and the bytes it read are forwarded unchanged.
- **Listeners**: the accept queue (`listen` backlog) is `/proc/sys/net/core/somaxconn` when
  readable and valid, else 1024.
- **TLS**: outbound TLS is intercepted or passed through as §5 decides; inbound TLS and every
  passed-through connection go through untouched. The SNI of the ClientHello is read for logs
  and metrics. The decrypted stream of an intercepted connection follows the rules of this
  section (detection, HTTP/1 codec, `h2` relay, failures, close and reset propagation,
  shutdown) with TLS in between: a FIN becomes `close_notify` then FIN, a FIN received without
  `close_notify` (a peer that just closes its socket) is read as a FIN, not as a failure, a reset
  stays a TCP reset on the other socket, and the bytes compared are the decrypted ones.
- **HTTP/2 prior knowledge (`h2c`)** connections are parsed by `hyper`. Each client
  connection gets its own connection to the original destination, and every stream is
  forwarded there as received. The proxy never routes on `:authority`/`Host`, never retries,
  adds no header (no `Date`, `Server`, `Via`, `X-Forwarded-*`), and adds no
  `content-length` that the client did not send. Traffic management is v2 (§1.2).
- **HTTP/1.x** connections go through mimic's own byte relay (the `http1` codec), not
  `hyper`. Each client connection gets its own connection to the original destination. Every
  byte read from one side is written to the other unchanged, in order, as soon as it is read:
  request and status lines, header names (case), order, duplicates and values, line endings,
  `obs-fold`, bodies and framing (`content-length`, `chunked` with chunk extensions and
  trailers, close-delimited responses), HTTP/1.0 and keep-alive, pipelining, `1xx` responses
  (`100 Continue`, `103 Early Hints`), `HEAD`/`204`/`304`. Nothing is added, removed or
  re-framed, and connection management (keep-alive, `Connection: close`) is left to the two
  ends. The codec parses only to find message boundaries and to fill the access log and the
  metrics; it lives in `mimic-core` (pure, no I/O) and keeps bounded state:
  - message framing follows RFC 9112 §6: a request has a body only with `content-length` or
    `transfer-encoding: chunked`; a response to `HEAD`, a `1xx`, `204` or `304` has none;
    otherwise `chunked` when it is the last transfer coding, else `content-length`, else the
    body runs until the destination closes;
  - a request line, status line or header line ends with CRLF or a bare LF (RFC 9112 §2.2);
  - a head over 64 KiB or 256 lines, a chunk-size line over 4 KiB or a trailer section over
    64 KiB or 256 lines is not framed (below);
  - responses are matched in order with the requests read; at most 32 requests are read
    ahead of their responses, after which the relay stops reading the client until a
    response completes (TCP back-pressure, no refusal);
  - a response that arrives while no request is pending (e.g. a `408` before the destination
    closes) is forwarded and logged as an exchange without `method` and `path`.
- **Unframed HTTP/1.** The codec never answers and never rejects. When it cannot frame a
  message unambiguously, the bytes still go through unchanged: the destination (or the
  client, for a response) interprets them, as it would without mimic. A message is unframed
  when its head does not parse (invalid tokens, a bare CR, `obs-fold` in a framing header),
  is over the limits above, or has an ambiguous length: both `content-length` and
  `transfer-encoding`, several `content-length` values that differ or one that is not a
  decimal number, a `transfer-encoding` with a coding that has parameters (`chunked;x=1`:
  recipients disagree on it) or quotes, a request `transfer-encoding` whose last coding is
  not `chunked` or that comes with HTTP/1.0, or an invalid chunk size. The exchange then
  writes its access line at once (`result: unframed`, with the fields known so far), and the
  rest of the connection is relayed as an opaque tunnel, with no further exchange line.
- **Load marker (F11, inbound only, with `MIMIC_LOAD_ENABLED=1`)**. This is the one place
  the HTTP/1 relay is not byte-exact, and only for requests that carry `mimic-load-run`
  (§7.8). On a pod without `MIMIC_LOAD_ENABLED` (load testing disabled in the chart) there is
  no load path at all: no hold-back, no marker check, `mimic-load-run` is an ordinary header
  relayed unchanged, and `GET /load` on the admin port answers `404`. With it, the relay reads
  each inbound request head whole before it forwards the first byte of it, one request at a
  time (a pipelined later head is judged when its turn comes). A head that carries one
  `mimic-load-run` header is rewritten in place: that header line is removed, nothing else of
  the head changes, and its body, framing, `Upgrade` and `CONNECT` are relayed unchanged. The
  head of the final response (or `101`) to such a request loses every `x-mimic-load` line
  and gains `x-mimic-load: ok`; interim `1xx` responses are untouched. A refused request is
  answered by the sidecar (`599`, `content-length: 0`, `connection: close`) once the answers to
  the earlier requests of the connection were written, the connection to the app is closed
  without a byte and the client connection is closed after the answer. The following are
  forwarded unchanged and are not load requests: a head that reaches 64 KiB without its end,
  a head the codec cannot frame (a header line without a colon, a name that is not a token such
  as `mimic-load-run : t`, `obs-fold`), a head not complete after 30 s without a byte (the
  detection stall above), and a head cut by the client's close. A request without the header
  is relayed as always.
- **Upgrades**: a request answered with `101` (WebSocket, `h2c` upgrade), or a `CONNECT`
  answered with 2xx, continues as an opaque tunnel after the response. After a request that
  asks for an upgrade or is a `CONNECT`, the relay reads no more client bytes until the final
  response to it arrives (the client must wait for it, RFC 9110 §7.8, §9.3.6). A declined
  upgrade leaves a plain HTTP/1 connection. The relay holds at most one 16 KiB read, the rest
  stays in the socket; a codec fed more than 64 KiB behind an unanswered upgrade (without the
  relay's flow control) reports the upgrade request as `unframed`, drops its
  copy and uses every byte: none is lost.
- **Failures**: the sidecar never synthesises a response (in discovery; mock answers are §8's). When the destination refuses the
  connection, the client connection is closed without any byte; when it closes or resets
  before answering, the client sees the same close or reset (HTTP/1), or the stream is reset
  (HTTP/2). An HTTP/2 client that breaks the protocol at connection level gets `GOAWAY` (the
  `h2` stack) and one `rejected` access line.
- **Idle and long-lived connections**: no idle, header-read, handshake or request timeout and no
  cap on streams. A connection lives until one side closes it. Two bounds protect the sidecar
  from clients that pin it: a connection whose first bytes are an incomplete prefix of a known
  protocol (an HTTP method, a TLS record header, the h2 preface) and that then stays silent on
  both sides for 30 s is reset (a connection with no byte at all is not a prefix: a
  server-first protocol is waited for); and at most `MIMIC_MAX_CONNECTIONS` connections are
  served at once in each direction (inbound and outbound have a budget of their own, so that a
  full inbound side cannot make the app's outbound calls wait for the requests they serve),
  further ones waiting in the accept backlog (never refused). The stall timer restarts on
  every read: a client that drips one byte at a time keeps its connection (accepted limit; the
  cap bounds the damage). The HTTP/2
  handshake with the destination does not wait for the destination: a silent destination
  leaves the client's streams pending, as it would without mimic, until a side closes.
- **Close and reset propagation** (HTTP/1 and tunnels): a FIN from one side reaches the other
  as a FIN once every byte read before it has been written; the other direction keeps
  flowing, so a destination can still answer a client that half-closed. A reset from one side
  reaches the other as a reset (`SO_LINGER` 0), also after a FIN: once a side has sent FIN,
  the relay watches its socket for an error (`EPOLLERR`) without reading it. An exchange whose
  client has closed and whose destination then closes without answering ends as
  `client_abort`. A client that closed and vanished (no RST, e.g. its host is gone) is noticed
  by TCP keepalive on its socket, which ends the connection like a reset. On HTTP/2, the
  client closing the connection closes the destination connection.
- **Resources**: TCP keepalive (idle 300 s, interval 30 s, 4 probes) is set on both sockets so
  that connections whose peer vanished are released. At start the sidecar raises its
  open-file soft limit to the hard limit, or to `fs.nr_open` (`/proc/sys/fs/nr_open`, else
  1048576) when the hard limit is unlimited; when `accept` fails (e.g. no descriptor left) it
  counts the error, logs it at most once per second and retries, without touching
  established connections.
- **Overload: forward, never refuse.** No admission control. Log lines go through a bounded
  in-memory queue; when stdout cannot keep up, lines are dropped and counted
  (`mimic_log_lines_dropped_total`) instead of slowing traffic.
- **Shutdown** (`SIGTERM`, sent after the app containers stopped): listeners stop accepting,
  HTTP/1 connections close (FIN to both sides, no byte added) once no request is pending and
  no message is half-read, HTTP/2 connections get `GOAWAY`,
  tunnels run until `MIMIC_SHUTDOWN_GRACE` ends (with report shipping enabled, §7.2, until
  three quarters of it at most: the last quarter is kept for the flush). When it ends, the sidecar closes every
  connection still open (tunnels, connections in detection, exchanges still pending), writes
  the access line of each (`result: shutdown`), then one WARN line `grace period over` with
  `connections` (how many it closed), and exits 0. An idle HTTP/1 connection is closed with a
  FIN, never a reset: the client bytes that reached the socket after the connection was found
  idle are read and discarded (at most 250 ms) before the sockets are released. The bytes
  protocol detection had already read when `SIGTERM` came are forwarded.
- **Access logs** (`target` = `access`, level INFO): one line per HTTP exchange (`message`
  `http exchange`) and one per opaque or TLS connection (`message` `connection`), written when
  it ends.

  | Field | HTTP exchange | Connection |
  |---|---|---|
  | `direction` | `inbound` \| `outbound` | same |
  | `protocol` | `http1` \| `h2` | `tls` \| `tcp` |
  | `dst` | original destination `ip:port` | same |
  | `authority` | normalised target authority (below), else `dst` | — |
  | `sni` | — | TLS only, when present |
  | `method`, `path` | method (at most 64 bytes); path **without query**, at most 1024 bytes | — |
  | `status` | response status, absent when none | — |
  | `grpc_status` | HTTP/2 only: from the trailers, when present | — |
  | `upgrade` | `true` when the exchange became a tunnel | — |
  | `tls` | `true` when the exchange was decrypted by interception (§5) | — |
  | `interception` | — | outbound TLS only: the §5 decision other than `intercepted` (an intercepted connection writes exchange lines instead) |
  | `result` | `ok`, `upstream_reset`, `client_abort`, `unframed` (HTTP/1 the codec cannot frame), `rejected` (HTTP/2 connection error), `shutdown` (closed at the end of the grace) | `ok`, `upstream_reset`, `client_abort`, `shutdown`, (F10) `mock_closed` (§8) |
  | `mock`, `mock_miss` | (F10) outbound in mock only: `exact` \| `endpoint` \| `fallback` \| `miss`, and the miss reason (§8) | — |
  | `duration_ms`, `head_ms` | until the response end; until the final (non-`1xx`) response head; both absent for `unframed` and `rejected` | `duration_ms` |
  | `bytes_in`, `bytes_out` | request / response body bytes | client → destination / destination → client |

  The target authority is, for HTTP/1, the authority of an absolute-form request target, else
  the `Host` header; for HTTP/2, `:authority`, else the `Host` header (RFC 9110 §7.2,
  RFC 9113 §8.3.1); its default port is 80, or 443 on an intercepted TLS connection. The HTTP/1
  codec keeps at most 64 bytes of the method and 1024 bytes of the `Host` value and of the
  authority of the request target (a bound on its memory), and the logged `method` and
  `authority` are cut accordingly. Header values, cookies, query strings and bodies are never
  logged, decrypted or not.

Accepted limits of the proxy:

- HTTP/2 connection settings (window sizes, concurrent streams, header list size) are the
  sidecar's, not the destination's. An HTTP/2 request that the `h2` stack refuses at stream
  level (`RST_STREAM`, e.g. a connection-specific header) never reaches the sidecar's code and
  writes no access line.
- The HTTP/1 codec only frames; it does not judge. Where it and the destination disagree on
  framing (a destination that rejects a bare LF the codec accepts, or a lenient one that
  accepts what the codec leaves unframed), the bytes are still exact, but the access lines
  and metrics of that connection can be wrong or missing from that point on (`unframed`).
- The relay reads at most 32 HTTP/1 requests ahead of their responses and holds the client's
  bytes after an upgrade or `CONNECT` request until its final response: this changes timing
  only, never bytes.
- A client that half-closes and stays alive while its destination never answers nor closes
  keeps the connection open (one task, two sockets), as the destination would keep its own
  socket without mimic. A client that vanished without a reset is noticed only by TCP
  keepalive (about 5 minutes after the last activity).
- An HTTP/1 request whose first bytes arrive after `SIGTERM` found its connection idle is
  not forwarded: the client sees the connection closed without a response and no access line
  is written. The app containers have already stopped then, so the client would get no
  answer without mimic either.
- WebSocket over HTTP/2 (extended `CONNECT`, RFC 8441) is not advertised, so clients use
  HTTP/1.1 for it.
- Inbound HTTPS, and outbound HTTPS that §5 passes through, are seen at connection level only
  (SNI, bytes, duration).
- From F4 the access-log `path` goes through the value detectors of §6 redaction (a JWT or a
  card number in a path segment is logged as `<redacted>`); a secret that no detector
  recognises (an opaque random token without a known prefix) is still logged.
- A connection on which neither side ever sends a byte stays in detection (it costs one task
  and two sockets, as an idle tunnel does).
- A client-first, non-HTTP protocol whose first bytes are only a token that could start an
  HTTP request line (for example `ping` with no space or line end) stays in detection until it
  sends more: such a client that waits for a reply first is stalled. Protocols that end their
  first message with a line end or send binary data are not affected.

## 5. TLS interception

Outbound TLS is intercepted so that HTTPS is seen as HTTP (§4.1): the sidecar terminates the
app's TLS with a certificate it issues for the requested name, and opens its own TLS
connection to the original destination. Inbound TLS is never intercepted (the sidecar has no
key of the app) and is passed through as in §4.1. Whenever interception would change what the
client or the destination sees, the connection is **passed through** untouched instead
(fail-safe): mimic never vouches for a certificate it could not verify.

- **Order of a connection.** On an outbound ClientHello the sidecar decides, in this order
  (first match wins), from the ClientHello, the original destination and its own state:

  | Case | Decision (`interception`) | Remembered |
  |---|---|---|
  | the ClientHello has an SNI that is not a valid DNS name (at most 253 bytes, labels of 1 to 63 letters, digits, `-` or `_`, not starting or ending with `-`) | `unsupported` | no |
  | SNI (or, without SNI, the destination IP) matches `MIMIC_TLS_EXCLUDE` | `excluded` | — |
  | destination remembered as passed through | the remembered reason | — |
  | the sidecar has no valid intermediate CA (below) | `no_ca` | no |
  | the ClientHello offers no TLS 1.2 or 1.3, or offers ALPN protocols none of which is `h2` or `http/1.1` | `unsupported` | no |
  | the sidecar's own handshake with the destination fails at TLS level: certificate not verified (unknown issuer, expired, wrong name), alert, no common version or cipher | `upstream_untrusted` | yes |
  | the destination asks for a client certificate (`CertificateRequest`) | `client_cert_requested` | yes |
  | the client answers the sidecar's certificate with an alert, or closes the connection right after it | `client_rejected` | yes |
  | otherwise | `intercepted` | — |

  The sidecar handshakes with the destination **before** answering the client: it offers the
  client's SNI and the client's ALPN list restricted to `h2` and `http/1.1` (client order),
  verifies the destination's chain against its upstream roots (below) and the SNI (the
  destination IP when there is no SNI), then completes the client's handshake with the ALPN
  the destination chose (none if it chose none). The decrypted client stream then goes through
  the same detection and the same relays as cleartext (§4.1): HTTP/1 through the byte-exact
  `http1` codec, HTTP/2 through the `h2` relay, anything else as an opaque decrypted tunnel.
  As in §4.1, the detection also reads the destination: one that speaks first (a TLS banner
  protocol) makes an opaque decrypted tunnel at once, and a client that stays silent is waited
  for, as without mimic.
- **Pass-through** means what §4.1 does for TLS today: a new TCP connection to the original
  destination receives the client's bytes unchanged, from the original ClientHello on, and the
  client sees the destination's own certificate. For `upstream_untrusted` and
  `client_cert_requested` the connection being handled is passed through at once (the
  sidecar's own TCP connection to the destination is closed); for `client_rejected` the
  connection being handled is lost (the client gave up on it) and the next one passes through.
  An I/O failure (reset, close) of the destination during the sidecar's handshake reaches the
  client as that reset or close, and nothing is remembered. Likewise a handshake with the
  client that ends otherwise than by an alert or a close (a reset, which may be a client crash,
  a timeout, a malformed message) drops that connection and remembers nothing.
- **Remembered destinations**: key = (SNI, else destination IP; destination port), kept in
  memory for the life of the process, at most 4096 (the oldest is forgotten first). The first
  time a destination is remembered the sidecar logs one WARN `tls passthrough` (`target`
  `tls`, fields `sni`, `dst`, `reason`).
- **CA hierarchy and custody.**
  - The controller keeps one random 32-byte **seed** in the Secret `<release>-tls-ca` of its
    namespace (created empty by the chart, filled by the controller at first start, deleted by
    `helm uninstall`). The seed never leaves the controller, which re-reads the Secret every
    60 s; concurrent replicas converge on one seed (optimistic concurrency).
  - Each deployment has its own **root CA**, derived from the seed (HKDF-SHA256 over
    `<namespace>/<deployment>`, ECDSA P-256), subject `CN=mimic <namespace>/<deployment>`,
    `O=mimic`, valid from the seed's creation time minus 1 h for 10 years. Its key is computed
    in the controller's memory when needed and never stored or sent. The root certificate is
    public: the webhook writes it into the pod spec (§7.1).
  - Each sidecar generates an **intermediate CA key** (ECDSA P-256) in memory at start and
    asks the controller to certify it (`POST /v1/tls/intermediate` on the sidecar API, §7.2).
    The intermediate (`CA:true`, `pathLen:0`, key usage `keyCertSign`) is valid from 1 h
    before issuance for 25 h; the sidecar renews it when less than half of its validity
    remains, retrying failures with a backoff from 1 s doubling to 60 s. It accepts an
    intermediate only if it chains to the root given in `MIMIC_TLS_CA`; otherwise it logs an
    ERROR `tls ca mismatch` and does not intercept.
  - **Leaf certificates** are issued in memory by the sidecar for the client's SNI (DNS SAN),
    or for the destination IP (IP SAN) when the ClientHello has no SNI, with one leaf key per
    process, the intermediate's validity window, and the intermediate in the chain. They are
    cached per name (at most 4096, oldest first) until the intermediate changes.
  - Every mimic certificate carries a Subject Key Identifier, and every one below the root an
    Authority Key Identifier equal to its issuer's (RFC 5280 §4.2.1.1–2): strict verifiers,
    such as Python 3.13's default, reject a chain without them.
  - Without a valid intermediate (none yet, expired, mismatch) every outbound TLS connection
    is passed through (`no_ca`). The sidecar's readiness does not depend on the controller.
  - No key, seed or token is ever logged, written to disk, sent to Valkey or exposed on an
    endpoint. The sidecar disables core dumps (`RLIMIT_CORE` 0) and makes itself non-dumpable
    (`PR_SET_DUMPABLE` 0) before it generates a key.
- **Upstream roots** (what the sidecar verifies destinations against): the system store of the
  mimic image, plus every `image-ca.pem` written by `mimic-trust` for the pod's app images
  (§7.1), loaded at the first interception; the mimic roots are never in them. A destination
  whose certificate the app's own images trust is therefore verified, and a destination signed
  by a CA that mimic does not know is passed through, so that the app judges it itself. A file
  that cannot be read, or a PEM block of it that does not parse, is skipped with one WARN
  `tls roots skipped` (`path`, `skipped`) at load; a load that fails is not remembered (the
  next interception loads again; the connection that met the failure is reset).
- **Trust volume**: the sidecar publishes the `mimic-trust` binary and the fallback store into
  `MIMIC_TRUST_DIR` at start (§7.1). If it cannot, the app images' bundles will not exist and
  the apps would not trust the mimic root: it logs an ERROR `trust volume not published` and
  does not intercept (`no_ca`).
- **The app trusts its deployment's root without code change**: `mimic-trust` (§7.1) builds,
  for each app image, a bundle of that image's own trust store plus the root, and the webhook
  points the usual variables at it: `SSL_CERT_FILE` (OpenSSL, Go, Ruby, .NET),
  `REQUESTS_CA_BUNDLE` (Python `requests`), `CURL_CA_BUNDLE`, `NODE_EXTRA_CA_CERTS` (Node.js)
  and a PKCS12 truststore through `JAVA_TOOL_OPTIONS` (JVM). Internal CAs installed in the
  image keep working, for the app and for the sidecar's upstream verification.
- **Exclusions**: `spec.tls.exclude` on the `MimicWorkload` lists names never intercepted
  (internal services, mTLS to a provider): an exact host (`api.example.com`), a suffix pattern
  `*.example.com` (any name below `example.com`, not `example.com` itself) or an IP literal
  (matched against the destination IP of a ClientHello without SNI). Entries are compared
  after the authority normalisation of §4.1 (case, trailing dot); at most 256 entries. The
  webhook gives the sidecar the sorted, deduplicated union of the lists of every
  `MimicWorkload` naming the deployment, in `MIMIC_TLS_EXCLUDE`; a union of more than 256
  entries is cut to its first 256 in sorted order, with one WARN `tls exclude truncated`; a change applies to pods
  created afterwards (`kubectl rollout restart`), like the injection itself.
- **Mock mode (F10)**: the sidecar answers intercepted connections without connecting to the
  destination (it issues the leaf for the SNI, or for the destination IP without SNI, and
  picks `h2` when offered, else `http/1.1`), and a connection that the table above would pass
  through before any upstream handshake (`unsupported`, `excluded`, a remembered destination,
  `no_ca`) is closed instead, without any byte towards the destination (the real destination
  is never called, §3), unless its SNI (or destination IP without SNI) matches
  `MIMIC_MOCK_PASSTHROUGH` (§8), in which case it is handled as in discovery. The decisions
  that need the destination (`upstream_untrusted`, `client_cert_requested`) cannot arise in
  mock; `client_rejected` closes the connection as in discovery (and remembers the
  destination). Every close by mock counts `mimic_tls_connections_total` under its decision
  and `mimic_mock_connections_total{decision="closed"}`, and writes a `connection` line with
  `result: mock_closed` and its `interception`. A decrypted stream that does not begin with
  an HTTP/1.x request line or the HTTP/2 preface is closed the same way (no `interception`);
  one that does is a mocked connection (`decision="mocked"`). In mock a leaf is issued for a
  name whose real certificate was never verified: it is only trusted by the pods of this
  deployment (its root is per deployment) and no traffic leaves, so it vouches for nothing
  outside the pod (§11.1).

## 6. Discovery

The sidecar turns the HTTP exchanges it relays into a description of the endpoints, keeps that
description in a bounded in-memory **local report**, and (from F5) ships it to the controller,
which merges the reports of every pod of the deployment in Valkey. F4 records **inbound**
exchanges (the endpoints the deployment exposes); F7 runs the same engine on outbound ones
(the APIs the deployment calls, §6.1), kept in the sidecar until F8, which ships them with the
inbound part and merges them per deployment (§6.1 "Shipping", §7.3).
Everything below except the capture tap, content decoding and the recorder thread is pure logic in `mimic-core`
(`discovery` module): parsing, templating, inference, redaction, sampling, merging.

- **What is recorded.** Every inbound HTTP/1 and `h2` exchange that got a response head, except
  probe requests (§4, they never reach the inbound listener), `unframed` and `rejected`
  exchanges, and exchanges whose path is over 2048 bytes or 32 segments (`invalid`). An
  exchange answered `404` or `405` creates no endpoint: it is counted on the endpoint whose
  template already matches it, else dropped as `not_found` (scanners must not fill the
  report). Inbound HTTPS is not decrypted (§5), so it is not discovered. An upgraded exchange
  (`101`, WebSocket) is recorded without bodies. Outbound exchanges follow §6.1 (`404` and
  `405` included).
- **Endpoint key**: `direction`, `port`, `method`, path template. For inbound, `port` is the
  original destination port (the app's listening port), not the `Host`: clients reach the
  same API through the Service name, its FQDN or the pod IP. The normalised authorities seen
  (§4.1) are kept per endpoint as a set of at most 16 (`hosts`, first 16 in byte order).
  Outbound endpoints are keyed by the destination's authority instead of a port (§6.1).
- **Path templating.** The path (without query) is split on `/`; segments are compared as
  raw bytes (no percent-decoding, case kept; an empty last segment, i.e. a trailing slash, is
  a segment). Per position, a segment is a **parameter** when its value is id-like: digits only;
  a UUID (8-4-4-4-12 hex, any case); a ULID (26 Crockford base32); hex of at least 8 characters
  with a digit; an RFC 3339 date or date-time; at least 16 characters with a letter and a
  digit; a value caught by a redaction detector (below); or anything containing `@`. Other
  segments are literals. Templates form a tree: below one node, at most
  `MIMIC_DISCOVERY_MAX_LITERALS` distinct literal children are kept; when one more appears the
  node becomes **absorbing**: its literals and its parameter child merge into one parameter, and
  every later segment at that position is that parameter. All parameter kinds at one position
  are one parameter (OpenAPI cannot hold `/a/{x}` and `/a/{y}`). The tree keeps no parameter
  value: the OpenAPI document (§7.6) derives a parameter's schema from the concrete paths of the
  endpoint's samples. A parameter is named after the previous literal segment, `{<literal>_id}`
  (lower case, characters other than `a-z0-9` replaced by `_`), or `{param<n>}` (n = 1-based
  segment position) when there is no previous literal or the name is taken. The name comes from the immediately
  preceding segment only, and only when that segment is a non-empty literal. There is one
  template tree per (direction, port), built from paths only; methods are keys below a template
  and do not shape the tree. gRPC paths
  (`content-type: application/grpc*`) are never templated, but they are redacted like any
  path (a client chooses its `content-type`). The result depends only on the set
  of paths seen, not on their order.
- **Schemas** (JSON Schema 2020-12 subset, the dialect of OpenAPI 3.1): `type` (set, with
  `null`; `integer` when every number seen is integral), `format` when every string value has
  it (`uuid`, `date-time`, `date`, `email`, `uri`, `ipv4`, `ipv6`), `properties`, `required`
  (present in every object seen), `items` (merged over the first 256 items of each array),
  `additionalProperties`. An object with more than 128 distinct keys becomes a map
  (`additionalProperties` = merge of the values, no `properties`); deeper than 16 levels a
  value is `{}`. Each body schema keeps at most **512 nodes**, cut by depth: the schema
  carries a **cut level** (16 when new) and nodes deeper than it are `{}`. Recording or merging
  sets the cut level to the minimum of the inputs' levels and of the deepest level at which the
  merged schema has at most 512 nodes (level 0 is a single `{}`). A node counts 1 plus its
  children; an `additionalProperties` map counts 256 times its value, so a merge never has a
  lower count than its inputs and the result does not depend on the order of merges. Inferred per endpoint: **path parameters**; **query** keys (percent-decoded,
  value schema, `array` when repeated, `required`; at most 64 keys, the first in byte order);
  request and response **headers** (lower-case names, value schema, `required`; at most 64, the
  first in byte order; hop-by-hop and framing headers `connection`, `keep-alive`,
  `transfer-encoding`, `te`, `upgrade`, `content-length`, `host` and pseudo-headers are
  skipped). A **name** (query key, header name, JSON field name) longer than 128 bytes is
  recorded as its first 128 bytes, cut on a UTF-8 character boundary, when it is recorded;
  names that collide after the cut merge. Redaction is decided on the full name, before the cut.
  Request and response **bodies** per media type (`content-type` without
  parameters): JSON (`application/json`, `*/*+json`) and `application/x-www-form-urlencoded`
  get a schema; any other type is **opaque**: media type and size only. Bodies are decoded
  from `content-encoding` `gzip` (also `x-gzip`), `deflate` (a zlib stream, or a raw deflate
  stream when the zlib header is absent) before inference, on the recorder thread,
  reading at most 256 KiB + 1 byte of output (a corrupt stream is `invalid`, a longer output
  `too_large`: a small body cannot expand into memory); any other value, `br`, `zstd` or a list of
  codings included, counts as `encoding` (no Brotli decoder is linked: one less
  decompressor exposed to untrusted input) and makes the body opaque. The `h2` tee copies the
  same bytes as the HTTP/1 codec (up to 256 KiB per body, in the capture budget): the heads
  are written as HTTP/1 text for the recorder, an extended `CONNECT` is not captured, and a
  body whose stream did not end before the exchange was recorded is not counted. A body over **256 KiB** (encoded, or decoded) is not parsed (`too_large`):
  it contributes its media type and size only. Schemas are inferred from the original values
  in memory; they hold types and formats, never values.
  Schemas and volatility are inferred from the redacted values (a redacted subtree is a `string`); the JSON root is level 1, `+` in a query or form is a space, a repeated header is an `array` like a repeated query key, and at most 16 media types are kept per side.
- **Volatile fields** (values that change on every call: idempotency keys, timestamps, ids).
  Tracked per endpoint for query keys, headers and JSON/form body leaves (JSON pointer with
  array indices as `*`, e.g. `/items/*/id`), at most 128 fields (the first in byte order): `n`
  (exchanges that had the field: once per exchange, however many array items or repeated
  headers carry it) and a bottom-32 sketch of the 64-bit XXH3 hashes of the redacted values. The
  distinct count `d` is exact while the sketch holds fewer than 32 hashes, else estimated
  (`31 / max normalised hash`). A field is **volatile** when `n ≥ 3` and `d ≥ n / 2`. Redacted
  fields are always marked `redacted`, which mock matching (§8) and the load test treat like
  volatile.
- **Redaction**, pure, applied to everything the report keeps (samples, `hosts`) and to the
  access-log `path`, before it is stored, logged or sent. The marker is the constant string
  `<redacted>` (stable: same output for any secret, so no hash of a secret is ever kept).
  - *By name* (header name, query key, form field, JSON key): the name is split into tokens
    at every character that is not a letter or a digit (`-`, `_`, `.`, ...), at a change of
    case (`refreshToken` is `refresh`, `token`; `APIKey` is `api`, `key`) and between letters
    and digits, then compared in lower case. A name is redacted when a run of consecutive
    tokens, joined, equals a built-in name or one of `MIMIC_DISCOVERY_REDACT_NAMES` (with
    `-` and `_` removed), or that name followed by `s`. Built-in names: `authorization`,
    `proxyauthorization`, `cookie`, `setcookie`, `password`, `passwd`, `secret`, `token`,
    `apikey`, `auth`, `session`, `signature`, `credential`, `privatekey`, `cvv`, `cvc`,
    `cardnumber`, `pan`, `iban`, `ssn`. The short names `auth`, `pan`, `iban`, `ssn`, `cvv`
    and `cvc` match whole tokens only. Every other built-in name is also matched inside a
    single token, glued or not (`JSESSIONID`, `csrftoken`, `XAPIKEY`, `clientsecret`,
    `userpassword`), as a prefix, a suffix or in the middle. The names of
    `MIMIC_DISCOVERY_REDACT_NAMES` match whole tokens only. So `X-Api-Key`, `auth_token`,
    `Authorization`, `client_secret`, `Session-Id`, `tokens` and `JSESSIONID` are redacted,
    `company`, `author`, `panel` and `passenger` are not. The whole value (a JSON subtree
    included) becomes the marker, except: `authorization` and `proxy-authorization` keep their
    scheme (`Bearer <redacted>`) only when the first word of the value is in the allowlist
    below, matched case-insensitively and written in its registered spelling (`bearer x` gives
    `Bearer <redacted>`); any other value, scheme unknown or absent, becomes the marker as a
    whole, since the first word could be the credential. Allowlist: the IANA *HTTP
    Authentication Scheme Registry* (`Basic`, `Bearer`, `Digest`, `DPoP`, `GNAP`, `HOBA`,
    `Mutual`, `Negotiate`, `OAuth`, `PrivateToken`, `SCRAM-SHA-1`, `SCRAM-SHA-256`, `vapid`,
    `Concealed`) plus `NTLM` and `AWS4-HMAC-SHA256`; `cookie` keeps cookie names (`a=<redacted>; b=<redacted>`);
    `set-cookie` keeps the name and attributes.
  - *By value* (every other string: header values, query values, path segments, JSON strings
    and numbers): a JWT (three base64url parts, the first decoding to a JSON object with
    `alg`), a PEM block, a `Bearer `/`Basic ` credential, a 13–19 digit card number (spaces or
    dashes allowed) that passes Luhn, an AWS access key id (`AKIA`/`ASIA` + 16), and tokens with
    a known prefix (`ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`, `github_pat_`, `glpat-`,
    `xox[abprs]-`, `sk_live_`, `rk_live_`, `sk-`, and from F7 a Google API key: `AIza` followed
    by 35 characters of `A-Za-z0-9_-`); a URL's userinfo password. A path segment and a query value are percent-decoded for the
    detectors (the stored form of a segment that is kept stays as received; a redacted one
    becomes the marker): `%65yJ...` is a JWT, `Bearer%20x` a credential.
  - *URLs in header values* (`Location`, `Referer`, `Content-Location`, `Link: <url>; rel=next`,
    `Refresh`, any value or token that contains `://`, `/`, `?` or `#`, so relative URLs too) are redacted like a
    path plus query: query parameters by name and value, path segments by value, and the
    fragment like a query. Scheme, host and other text are unchanged.
  - Opaque bodies are never kept (only media type and size). Redaction counts appear in
    `mimic_discovery_redactions_total{location,rule}` (§9): a redaction inside a URL of a header
    value counts under `header`, and only the at most 64 headers the report keeps are counted.
- **Samples**: per endpoint at most **20**, each a redacted request (method, concrete path,
  query, headers, body) and response (status, headers, body), with its head latency. Each body
  is kept only when parsed and at most 256 KiB. Selection is deterministic and
  order-independent: candidates are deduplicated by the XXH3 hash of their redacted content;
  first one sample per status code (lowest status first), then one per class
  (status, request shape, response shape — a shape is the hash of the body schema skeleton),
  then the rest; within each pass the lowest content hash wins. All samples of a sidecar fit in
  `MIMIC_DISCOVERY_SAMPLE_BUDGET`; when it is full, new samples are not admitted (counted
  `budget`) and counters, schemas and volatility still update.
- **Counters and latency**, per endpoint: exchanges by status code, and a histogram of the
  time to the response head (`head_ms`, §4.1) with fixed log buckets (4 per doubling, from
  1 ms to 131 s, plus overflow), sparse.
- **Local report.** A JSON document (`"version": 1`; 2 when it holds outbound scopes, §6.1), serialised with sorted keys
  (`BTreeMap`), holding the endpoints above plus `pod` and the number of exchanges it covers.
  The wire format serialises the template tree with its absorbing flags, so a merge keeps
  absorbed positions absorbed. A merged `pod` is kept only when both reports have the same one,
  else it is null. After a merge, `hosts` of an endpoint keeps the 16 smallest in byte order.
  Merging two reports is a pure function, commutative and associative, that unions the
  template trees (with the absorbing rule), adds counters and histograms, merges schemas,
  sketches and `hosts`, and reselects samples: the sidecar records an exchange by merging a
  one-exchange report, and the controller (F5) uses the same function across pods. Merging is
  not idempotent for counters: the transport must apply each report once (F5). Counters
  saturate at `u64::MAX` (a report read back from Valkey is not trusted). The merge also
  enforces the limits it is given, so that the union of reports the controller does not trust
  stays bounded: past `max_endpoints` it keeps the endpoints that sort first by direction, port,
  template and method (the others, their counters included, are dropped, and the nodes left
  empty are pruned), and while the samples exceed `sample_budget` it drops the sample with the
  highest content hash (then the highest endpoint key). Both are functions of the merged
  content: the merge is commutative; it is associative as long as no dropped endpoint would
  have been absorbed by a later merge. `to_json` reports a serialisation error instead of an
  empty document.
- **Bounds.** At most `MIMIC_DISCOVERY_MAX_ENDPOINTS` endpoints per direction (§6.1; further new
  endpoints are dropped, `endpoint_limit`, one WARN `endpoint limit reached`); the template tree
  keeps its shape for the life of the process (counters and samples are what F5 ships and
  resets). Capture never slows forwarding: the proxy copies at most the head (64 KiB, §4.1) and
  256 KiB of each body into an exchange record while relaying, within the global in-flight
  budget `MIMIC_DISCOVERY_CAPTURE_BUDGET` (a body that does not fit is not captured,
  `budget`), and hands the record to a bounded queue of `MIMIC_DISCOVERY_QUEUE` records,
  drained by one recorder thread that does the decoding, inference and merge. A full queue
  drops the record (`dropped`). Forwarding never waits for discovery (§4.1: forward, never
  refuse).
- **HTTP/1 tap (F4 step 6).** In F4 only inbound exchanges of plain HTTP/1 connections are
  captured; F7 also captures outbound ones, cleartext and intercepted (§6.1). The codec hands the raw head and the first 256 KiB of the body payload (chunk
  framing removed) of each message to the exchange record; a head that does not fit in the
  capture budget drops the whole exchange (`dropped`), a body that does not fit is `budget`.
  A body of more than 256 KiB is `too_large` and contributes nothing (not even its size) to the
  report. An exchange whose response head came but whose request or response body did not
  complete is recorded without that body (not counted in `mimic_discovery_bodies_total`);
  an exchange that got no response head, a `CONNECT` and an `unframed` exchange are not
  recorded. Every recorded or dropped exchange has exactly one `mimic_discovery_exchanges_total`
  result; the other results are counted by the recorder, `dropped` by the tap. Bodies and
  samples are counted for recorded exchanges only. `content-encoding` other than `identity`
  makes the body `encoding` until the decoders of step 7. `mimic_discovery_samples_total`
  has `kept` and `budget` in F4 (a sample admitted to the candidates, or refused for the
  budget); `replaced` and `rejected` need the sample selection to report its choices
  (later). `mimic_discovery_redactions_total` is counted by the recorder from the same inputs
  and detectors as the report, for recorded exchanges. The authority of `hosts` is the
  normalised `Host` or absolute-form authority of §4.1; the endpoint port is the original
  destination port.
- **Logs** (`target` = `discovery`): INFO `endpoint discovered` (`direction`, `port`,
  `method`, `template`) once per new endpoint (not for the parameter a node absorbs into), logged
  when its exchange is recorded, one exchange at a time in arrival order, so an endpoint that a
  later exchange absorbs was logged before the absorption,
  INFO `template absorbed` (`direction`, `port`, `method`, `template` = the new template of
  the exchange, `old` = the replaced `<METHOD> <template>` entries, comma-separated, the first
  16, and `old_count`) when a node absorbs, WARN `endpoint limit reached` once, WARN `sample budget full` once when the budget
  becomes full, again only after the samples fell below 7/8 of the budget and it filled anew;
  ERROR `recorder panic` (counted `panic`) when the recorder fails on an exchange, which is
  skipped. Templates are redacted; no line holds a header value, query, body or
  sample.
- **Local access.** `GET /discovery` on the admin port answers the local report
  (`application/json`) only to clients on loopback (`127.0.0.1`, `::1`): `kubectl
  port-forward` (Kubernetes RBAC `pods/portforward` is the authentication) and the pod itself.
  Any other peer gets 403. Its content is the redacted report, as it would be shipped. A serialisation failure answers
  500 and logs ERROR `discovery serialisation failed` once.
- **Shipping (F5).** Every `MIMIC_DISCOVERY_PUSH_INTERVAL` and at `SIGTERM`, the sidecar
  posts the report accumulated since its last acknowledged post to the sidecar API (§7.2, same
  authentication, the deployment taken from the token's pod), with a per-pod sequence number;
  the controller merges it into the deployment's report in Valkey atomically with the
  sequence, so a retried post is applied once, and answers 200; the sidecar then drops the
  shipped counters and samples (the tree shape stays). Until acknowledged, reports keep
  merging locally within the same bounds. Replicas therefore merge only in the controller,
  with the same pure merge; the controller re-applies templating on the union, so a position
  that each pod sees below the literal limit but the deployment sees above it is absorbed.
  Key layout, retention (30 days) and the API are in §7.3 and §7.2. From F8 the push also
  carries the outbound part (§6.1 "Shipping").
- **Consumers.** F6 renders stored reports as OpenAPI 3.1 (§7.6). F7 records egress with the same engine.
  F10 mock matching ignores volatile and redacted fields (§8). F11 (load test) builds requests
  from ingress samples and volatility; redacted values (credentials) are supplied by the user
  from a Secret, and its own traffic, marked by `mimic-load-run`, is not recorded (§7.8).

### 6.1 Egress recording (F7)

The sidecar records the exchanges the app sends to other services and providers with the engine
of §6, into the same local report. Everything §6 states applies to outbound exchanges unless
this section says otherwise; the scope key, origin, tunnels and outbound redaction view are pure
logic in `mimic-core` (`discovery` module).

- **What is recorded.** Every outbound HTTP/1 and `h2` exchange that got a response head, on a
  cleartext connection or on a TLS connection that §5 intercepted (the decrypted exchange,
  through the same tap, `h2` tee, capture budget and queue as inbound), except `unframed` and
  `rejected` exchanges, `CONNECT` requests and `invalid` paths, as inbound. The sidecar's own
  traffic (probes, sidecar API calls) runs as UID 1337 and is never captured (§4). Unlike
  inbound, an outbound `404` or `405` is recorded like any other status: it creates its
  endpoint and is a sample candidate. The app chose the call, and a mock (F10) must answer it
  as the destination did; the bounds below contain an app that forwards paths chosen by its
  own clients.
- **Scope key: the authority.** Outbound endpoints are grouped by the destination's authority
  `<host>:<port>`, the first that applies of:
  1. the normalised target authority of §4.1 (absolute-form authority, else `Host`; for `h2`
     `:authority`, else `Host`), with its explicit port, else the default port (80, or 443 on
     an intercepted connection);
  2. when it is absent, `(invalid)` or a host outside the grammar below: the SNI of the
     intercepted ClientHello with the original destination port;
  3. else the original destination `ip:port`.

  `host` is a lower-case DNS name of at most 253 bytes (the SNI rule of §5), an IPv4 literal or
  a bracketed IPv6 literal in RFC 5952 form; `port` is decimal 1–65535 without leading zero,
  always written (a destination port 0, which no connection has, is written as 1, so that every key matches the grammar). A host caught by a value detector of §6 redaction is the marker
  (`<redacted>:443`). The authority, not the IP, is the key: a provider's IPs change with DNS,
  and one IP serves many hosts. Path templating, its literal limit and absorption are per
  (direction, scope): `/v1/payments` on two authorities are two endpoints. An outbound endpoint
  has no `hosts`; its scope has an **origin** instead: `schemes` (exchanges counted under
  `http` and `https`, `https` meaning intercepted TLS), `sni` (the distinct SNIs of its
  intercepted connections) and `dst` (the distinct original destinations `ip:port`), each set at
  most 16, the first in byte order.
- **Unrecordable traffic.** What carries no plaintext HTTP is neither decrypted nor parsed. It
  is counted in the `tunnels` of an outbound scope, by kind, once per connection when its kind
  is decided, with no endpoint and no sample:
  - `tcp`: an opaque outbound connection (server-first or non-HTTP protocol, decrypted or not);
  - `connect`: an HTTP `CONNECT` answered `2xx` (an egress proxy); its scope is the request
    target authority;
  - `tls_<reason>`: an outbound TLS connection that §5 passes through, one kind per decision
    (`tls_excluded`, `tls_no_ca`, `tls_unsupported`, `tls_upstream_untrusted`,
    `tls_client_cert_requested` for an mTLS destination, since the sidecar has no client key, and
    `tls_client_rejected` for a pinning client or one with its own truststore).

  The scope of a TLS tunnel is the SNI with the destination port when the SNI is valid, else the
  destination `ip:port`; of a `tcp` tunnel, the SNI of an intercepted connection with the
  destination port, else the destination `ip:port`. These counts show which destinations cannot
  be documented, nor mocked by F10.
- **Latency as the app sees it.** `head_ms` of §4.1: from the first byte of the request read
  from the app to the final response head read from the destination, which the relay forwards
  to the app at once. The same histogram as inbound. Connection setup (the app's DNS lookup, its
  TCP connect, and the TLS handshake with the destination that §5 completes before answering
  the app) happens before the request and is not in it (§11.1).
- **Outbound view of redaction.** An outbound request carries the app's own credentials (one
  provider key opens the whole account), often under names no rule knows. In an outbound
  **request** only, besides every §6 rule:
  - a header value is kept only for `accept`, `accept-charset`, `accept-encoding`,
    `accept-language`, `cache-control`, `content-encoding`, `content-language`, `content-type`,
    `expect`, `if-match`, `if-modified-since`, `if-none-match`, `if-range`,
    `if-unmodified-since`, `pragma`, `range`, `user-agent`, `grpc-accept-encoding`,
    `grpc-encoding` and `grpc-timeout` (still through the value detectors); `authorization`,
    `proxy-authorization` and `cookie` keep their §6 rules (scheme, cookie names); every other
    header value becomes the marker (rule `outbound_header`, its field marked `redacted`), so
    `Ocp-Apim-Subscription-Key` or `X-Tenant-Key` keep their name and lose their value;
  - a query key, form field or JSON key with `key` or `sig` as a whole token (`?key=`, `sig`,
    `idempotency_key`) is redacted by name.

  Responses of outbound exchanges, and inbound exchanges, follow §6 alone.
- **Bounds and shedding.** Outbound shares the capture budget, the queue and the recorder
  thread with inbound: under overload records are dropped and bodies skipped (`dropped`,
  `budget`) in proportion to each direction's traffic, and traffic is never slowed or altered
  (§4.1). `MIMIC_DISCOVERY_MAX_ENDPOINTS` applies to each direction on its own: outbound
  endpoints never refuse an inbound one, nor the reverse; an outbound scope whose endpoints are
  all evicted stays, with its `origin` and `tunnels`, so that the merge does not depend on the
  order of the evictions. At most
  `MIMIC_DISCOVERY_MAX_AUTHORITIES` outbound scopes are held: an exchange or a tunnel to one more
  is refused (`authority_limit`, one WARN `authority limit reached`); there is no host
  templating (§11.1). Outbound samples hold at most half of `MIMIC_DISCOVERY_SAMPLE_BUDGET`
  (`budget` beyond it), so egress never takes the room of the API the deployment serves;
  inbound samples may use the whole budget, as in F4.
- **Report format, version 2.** Outbound scopes are under `scopes.outbound`, keyed by
  authority, each with its tree, gRPC paths, `origin` and `tunnels`. A report is
  `"version": 2` if and only if it holds an outbound scope keyed by an authority, in `from_json`,
  the push check and serialisation alike; otherwise it is `"version": 1` and byte-identical to
  F4/F5 (stored reports and pushes do not change). From F7 `Report::from_json` reads both and
  rejects a version that does not follow this rule, an inbound key that is not a port, an
  outbound key outside the grammar above (so an outbound scope keyed by a port is no valid
  report), `origin` or `tunnels` on an inbound scope, an `origin` with more than 16 `sni` or
  `dst`, a scheme other than `http` and `https`, and a `tunnels` kind outside the list above. The merge adds `schemes` and `tunnels`, keeps the 16 smallest
  `sni` and `dst`, and past `max_authorities` keeps the authorities that sort first (the
  sidecar trusts its own report; the store ranks by traffic, F8).
- **Outbound exchange counter (F8).** A report has one `exchanges` counter for both
  directions. A version 2 report also carries `outbound_exchanges`, the exchanges of its
  outbound scopes (at most `exchanges`), so that the split of §7.3 is exact; it is present if
  and only if the report is version 2, so a version 1 report stays byte-identical to F4/F5. The
  recorder counts it for each recorded outbound exchange (a tunnel is no exchange), the merge
  adds it (saturating), and the structural check rejects a version 1 report that carries a
  non-zero one and a version 2 report whose counter exceeds `exchanges` (`malformed`). An
  absent counter in version 2 reads as 0. When a report loses its last outbound scope (eviction, expiry or any other removal) it
  drops to version 1 and the same step subtracts `outbound_exchanges` from `exchanges`
  (saturating) and resets the counter to 0: the removed exchanges go with their endpoints
  (already counted as evicted or expired), and the report is a valid version 1 with exact
  inbound counts.
- **Shipping (F8).** In F7 the outbound part was kept in the sidecar: never frozen, pushed nor
  reset, for the life of the process (pushes stayed `"version": 1`, inbound only). From F8 the
  sidecar freezes the outbound part together with the inbound one into the pending report of
  §7.2, so a push is `"version": 2` whenever it holds an outbound scope, and after the
  acknowledgement drops the shipped outbound counters, samples, `origin` and `tunnels` as it does
  for inbound (the tree shape stays). A report is frozen when it covers an exchange or a tunnel of
  either direction. `GET /discovery` then serves what is not acknowledged yet, in both
  directions. The outbound half of `MIMIC_DISCOVERY_SAMPLE_BUDGET` applies to the pending and
  the current report together. Without `MIMIC_CONTROLLER_URL` nothing is shipped and the
  outbound part accumulates as in F7. A controller up to F7 answers `400` (`version`) to a
  version 2 report; the **fallback to inbound only** of §7.2 makes sure that this, a mixed set of
  controller replicas during a rollout, or a controller rollback never costs the inbound data:
  the inbound part is posted alone and the outbound part waits in the sidecar, within its
  bounds, for a controller that accepts it. The controller side (structural check with the
  outbound bounds, re-redaction with the outbound view, authority limit ranked by traffic,
  per-direction sample floor, the `:outbound` key, status, documents and endpoint list) is in
  §7.3 to §7.6. F9 serves the aggregated egress to sidecars; F10 mocks it.
- **Logs** (`target` = `discovery`): `endpoint discovered` and `template absorbed` of an
  outbound endpoint carry `authority` instead of `port`; WARN `endpoint limit reached` is
  written once per direction (`direction`); WARN `authority limit reached` (`limit`) once; WARN
  `sample budget full` carries `direction` (outbound: when its half fills). Authorities are
  logged, as in access lines; never a header value, query, body or sample.

## 7. Controller and storage

- `MimicWorkload` (namespaced CRD, `mimic.platform.io/v1alpha1`): selects the deployment named
  by `spec.deployment` (required, DNS-1123 name, immutable) in its namespace, holds
  `spec.mode` (`discovery` | `mock`, default `discovery`), `spec.tls.exclude` (F3, §5),
  `spec.discovery.redact` (F4, §6: extra names redacted by name, at most 64, each of `a-z`, `A-Z`, `0-9`, `-`, `_`,
  lower-cased by the webhook, applied like `spec.tls.exclude` to pods created afterwards),
  from F10 `spec.mock.latencyFactor` (number, 0–100, default 1, §8; the controller rounds it
  to the nearest hundredth, halves up) and
  `spec.mock.passthrough` (at most 256 entries of the `spec.tls.exclude` grammar, §8);
  `status` reports the deployment's discovery counts and pushing pods
  (F5, §7.4) and from F10 mode convergence. F1 introduces the CRD with `spec.deployment` and `spec.mode` only; other fields arrive
  with their feature. When several `MimicWorkload`s name the same deployment, any of them
  selects it for injection. **Effective mode** (F10, pure, `mimic-core` `mock::effective`):
  `mock` when at least one of them has `spec.mode: mock`, else `discovery` (a deployment
  injected by label without any `MimicWorkload` is in `discovery`); the latency factor is the
  `spec.mock.latencyFactor` of the first `mock` one in name order; `spec.mock.passthrough` is
  the sorted, deduplicated union over all of them (cut to its first 256, as `spec.tls.exclude`).
  Mock wins so that nobody's load test calls a real provider because another
  `MimicWorkload` still says `discovery`. *Upgrade and rollback* (F10): the CRD is a chart
  template (`deploy/helm/mimic/templates/crd.yaml`, kept on uninstall), not a `crds/` file, so
  `helm upgrade` applies the new schema with `spec.mock` and `status.mode`. `helm rollback` to
  a chart up to F9 applies the old schema: the API server then prunes `spec.mock` and
  `status.mode` from every read and from the next write of each object (a re-upgrade before a
  write gets them back from etcd), and the F9 controller sends no mode, so F10 sidecars keep
  their mode (§7.7) while pods recreated with the F9 image never mock (§11.1).
- From F11, `MimicLoadTest` (namespaced CRD, same group and version, only when the chart's
  `loadTest.enabled` is true): one load test of `spec.deployment` in its namespace, run by a
  `mimic-load` Job while the deployment is in mock, results in its `status` (§7.8).
- Sidecars push their local recordings periodically (§6 "Shipping", §7.2), from F9 pull the
  deployment's aggregated egress recordings (§7.7) and, from F10, its mode. Valkey keys
  are namespaced by `<namespace>/<deployment>` (§7.3).
- The controller merges endpoints of all pods of a deployment (same endpoint key → merged
  counters, histograms, schemas, sample sets) with the pure merge of §6, which also
  re-applies path templating on the union, within the store bounds of §7.3.
- The UI and its API are read-only and served on the controller's UI listener, bound to
  loopback and reached by `kubectl port-forward` (Kubernetes RBAC `pods/portforward` in the
  controller's namespace is the authentication, §7.5). The UI never sends traffic to the
  cluster's workloads: it reads the store only (§7.5, §7.6).

### 7.1 Injection

- The controller serves a mutating admission webhook (HTTPS, port 8443) for pod `CREATE`.
  Kubernetes ≥ 1.33 (native sidecars); the chart refuses older clusters (`kubeVersion`).
- **Selection** (label key `mimic.platform.io/inject`):

  | Namespace label | Pod label | Injected |
  |---|---|---|
  | any | `disabled` | never |
  | `enabled` | absent or `enabled` | yes |
  | `selected` | absent | only if a `MimicWorkload` in the namespace has `spec.deployment` equal to the pod's deployment |
  | any | `enabled` | yes |
  | absent | absent | no (the webhook is not even called) |

  The namespaces `kube-system`, `kube-public`, `kube-node-lease` and the controller's own
  namespace are never injected. The `MutatingWebhookConfiguration` has three webhooks, one per
  path, so the API server only calls the controller for pods in scope (namespace and object
  selectors, no namespace read by the controller): `/inject/namespace` (namespace `enabled`),
  `/inject/workload` (namespace `selected`), `/inject/pod` (pod `enabled`). A pod matched twice
  is handled once (idempotence below). Only pods of a Deployment are injected (owner
  ReplicaSet named `<deployment>-<pod-template-hash>`); the deployment name is the workload
  identity.
- **Admin port name** (#378, MON-03): the `mimic-sidecar` container declares `containerPort` 15100,
  `protocol: TCP`, named `mimic-admin`, so that a `PodMonitor` can scrape it by port name. The
  declaration is informational (no `hostPort`) and does not change the redirect rules.
- **Skipped** pods are admitted unchanged except for the annotation
  `mimic.platform.io/skipped: <reason>`: `host-network`, `unsupported-owner`, `port-conflict`
  (an app container declares port 15001, 15006 or 15100), `uid-conflict` (an app container or
  the pod sets `runAsUser: 1337`), `name-conflict` (an app container is named `mimic-init`,
  `mimic-sidecar` or `mimic-trust-<n>`, or an init container is named `mimic-init` in a pod
  without a `mimic-sidecar` init container, or the pod has a volume named `mimic-trust` or
  `mimic-token`, or an app container already mounts something at `/etc/mimic/tls`). `unsupported-owner` applies only when injection was asked by
  an `enabled` label (pod or namespace); in a `selected` namespace, an unlabelled pod not owned
  by a Deployment is not selected and stays untouched, without annotation. A pod that already has a `mimic-sidecar` init container is
  admitted unchanged (idempotent, `reinvocationPolicy: IfNeeded`).
- **Injected** pods get, in front of their own init containers: `mimic-init`, then
  `mimic-sidecar` (`restartPolicy: Always`, startup and liveness probes on its admin port), then
  one `mimic-trust-<n>` per distinct app image (from F3), and the annotation
  `mimic.platform.io/injected: <version>`. No mimic container mounts an app volume or the
  pod's API service account token. Every literal `env` value the patch writes has each `$`
  doubled, so the kubelet's `$(VAR)` expansion gives the container the value unchanged.
- **TLS trust wiring** (F3, §5). The patch adds two volumes: `mimic-trust` (`emptyDir`,
  `sizeLimit: 16Mi`) and `mimic-token` (projected service account token of the pod's own
  service account, `audience: mimic.platform.io`, `expirationSeconds: 3600`, path `token`;
  the API server rejects that audience, so the token is only good for the sidecar API).
  - `mimic-sidecar` mounts `mimic-trust` at `/mimic` (read-write; its own mount, in which it
    owns `/mimic/state`, where it saves its mode, §8, while `mimic-trust-<n>` writes only
    `trust/<n>` and the app containers only see a `trust/<n>` subpath, read-only) and `mimic-token` at
    `/var/run/secrets/mimic.platform.io` (read-only), and gets `MIMIC_TLS_CA` (the
    deployment's root certificate, PEM), `MIMIC_TLS_EXCLUDE` (§5), `MIMIC_CONTROLLER_URL`
    (`https://<webhook service>.<namespace>.svc:<sidecar API port>`) and `MIMIC_CONTROLLER_CA`
    (the webhook CA, PEM); from F10 also `MIMIC_MODE`, `MIMIC_MOCK_LATENCY_FACTOR` and
    `MIMIC_MOCK_PASSTHROUGH` (the effective mode, factor and passthrough list of §7 at
    admission; the last two absent when they are the default or empty, `MIMIC_MODE` always
    set); always `MALLOC_ARENA_MAX=2` and `MALLOC_MMAP_THRESHOLD_=131072` (bound the glibc
    allocator, MCK-18). Before it is ready it copies the static `mimic-trust` binary to
    `/mimic/bin/mimic-trust` and its image's system store to `/mimic/bin/fallback-ca.pem`.
  - `mimic-trust-<n>` (n = 0, 1, … in order of first appearance of the image among the init
    then regular app containers) runs that image with its `imagePullPolicy`,
    `command: [/mimic/bin/mimic-trust]`, UID/GID 1337, `runAsNonRoot`, read-only root,
    no capability, no privilege escalation, `RuntimeDefault`, requests `10m`/`16Mi`, limit
    `64Mi`, `mimic-trust` mounted at `/mimic`, and `MIMIC_TRUST_CA` (root PEM),
    `MIMIC_TRUST_OUT=/mimic/trust/<n>`, `MIMIC_LOG`. It runs mimic's binary, never the image's
    entrypoint, and opens no socket.
  - Every app container and app init container mounts `mimic-trust` with
    `subPath: trust/<n>` of its image at `/etc/mimic/tls` (read-only) and gets, unless its
    `env` already defines the variable: `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE`,
    `CURL_CA_BUNDLE` = `/etc/mimic/tls/ca-bundle.pem`; `NODE_EXTRA_CA_CERTS` =
    `/etc/mimic/tls/node-extra-ca.pem`; `JAVA_TOOL_OPTIONS` =
    `-Djavax.net.ssl.trustStore=/etc/mimic/tls/truststore.p12 -Djavax.net.ssl.trustStoreType=PKCS12 -Djavax.net.ssl.trustStorePassword=mimic`.
    A `JAVA_TOOL_OPTIONS` with a literal `value` gets those options prepended (the app's own
    options come later and win); a variable defined with `valueFrom` is left untouched.
    Besides these mounts and variables, only the `httpGet` probes of the app containers are
    changed (§4).
  - `mimic-trust` writes into `MIMIC_TRUST_OUT` (files `0644`): `image-ca.pem` (the image's
    store: the first existing of `/etc/ssl/certs/ca-certificates.crt`,
    `/etc/pki/tls/certs/ca-bundle.crt`, `/etc/ssl/ca-bundle.pem`, `/etc/ssl/cert.pem`,
    `/etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem`, plus the files named by the image's
    own `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE` and `CURL_CA_BUNDLE`; when none exists, the
    fallback store with a WARN `no image trust store`), `mimic-ca.pem` (the root),
    `ca-bundle.pem` (`image-ca.pem` then the root), `node-extra-ca.pem` (the file named by the
    image's own `NODE_EXTRA_CA_CERTS`, if any, then the root) and `truststore.p12` (every
    certificate of `ca-bundle.pem` as a trusted entry, password `mimic`). Certificates are
    deduplicated, in first-seen order; unparsable blocks are skipped and counted. It logs one
    INFO `trust bundle written` (sources, counts) and exits 0, 1 when it cannot write, 2 on an
    invalid variable.
- The webhook computes the deployment's root certificate from the TLS seed (§5); while the
  seed is not loaded the controller is unready and an admission that needs it is answered
  `allowed: false`.
- The webhook uses `failurePolicy: Fail`: while the controller is unavailable, pod creation in
  the selected scope fails (fail-closed, consistent with mock mode never calling the real
  destination). Pods outside the scope are never sent to the webhook.
- Injection happens at pod creation only: labelling a namespace or creating a `MimicWorkload`
  does not restart running pods; `kubectl rollout restart` does.
- **Webhook certificate, self-managed**: the controller keeps a CA (10 years) and a serving
  certificate (1 year, SAN `<service>.<namespace>.svc`) in the Secret `<release>-webhook-tls`
  (created empty by the chart), writes the CA into the `caBundle` of its
  `MutatingWebhookConfiguration` (watched: a `caBundle` changed by anyone else is restored
  within 10 s), and renews the serving certificate when less than 30 days
  remain, without changing the CA. Concurrent replicas converge on one certificate (optimistic
  concurrency on the Secret). A `ValidatingAdmissionPolicy` lets the controller's service
  account change nothing in that configuration except `caBundle`.
- **Webhook availability** (it fails closed, so a stalled webhook stalls pod creation in scope):
  the chart runs 2 controller replicas by default, spread across nodes (preferred pod
  anti-affinity), with a PodDisruptionBudget `maxUnavailable: 1`. The webhook listener serves
  at most `MIMIC_WEBHOOK_MAX_CONNECTIONS` connections at once (further ones wait in the accept
  backlog); a connection must complete its TLS handshake, and then each request (headers and
  body), within `MIMIC_WEBHOOK_REQUEST_TIMEOUT`, or it is closed; an idle keep-alive connection
  is closed after 60 s. A NetworkPolicy admits ingress to the controller only on the webhook
  port (from any source by default, since the API server's source address depends on the
  cluster; `networkPolicy.webhookFrom` narrows it) and on the admin port (from the release
  namespace and the namespaces selected by `networkPolicy.metricsNamespaceSelector`).
- The CRD carries `helm.sh/resource-policy: keep`: `helm uninstall` leaves the CRD and every
  `MimicWorkload`; removing them is explicit (`kubectl delete crd
  mimicworkloads.mimic.platform.io`). A later install with the same release name and namespace
  adopts the kept CRD.

### 7.2 Sidecar API

- HTTPS on `MIMIC_SIDECAR_API_ADDR` (port 8444) with the webhook serving certificate (SAN
  `<service>.<namespace>.svc`), same connection cap and timeouts as the webhook listener
  (§7.1) with its own budget. Sidecars verify it against `MIMIC_CONTROLLER_CA`.
- **Authentication**: `Authorization: Bearer <token>`, the projected token of §7.1, which the
  sidecar re-reads from its file for every request. The controller validates it with a
  `TokenReview` (audience `mimic.platform.io`) and takes the namespace from the service
  account user name and the pod name from the bound-pod claim
  (`authentication.kubernetes.io/pod-name`). A request names its deployment; the controller
  accepts it only if the pod name is one the ReplicaSet of that deployment can generate:
  `<deployment>-<pod-template-hash>-<5 chars>`, the part before the 5 chars truncated to 58
  characters, the hash made of the characters `bcdfghjklmnpqrstvwxz2456789`. No pod or
  ReplicaSet is read (§11.1).
- **Admission order** (F5, every endpoint): connection cap and timeouts; then a **per-source
  rate limit**, a token bucket per peer IP address of `MIMIC_SIDECAR_API_RATE` requests per
  second with a burst of twice that (at most 65 536 sources tracked, the least recently seen
  forgotten first): a request over it gets `429` with `Retry-After: 1` and causes no
  `TokenReview`; then authentication from the headers alone, so that a body is read only for
  an authenticated caller (the report endpoint, whose deployment is in its path, is also
  authorised before its body is read). A successful review is cached
  for 60 s (at most 10 000 entries, keyed by the SHA-256 of the token; the token itself is
  never kept); a failed one is not cached. A review not served from the cache counts against the
  `TokenReview` limits of `POST /v1/tls/intermediate`, which every endpoint shares (`503` when
  spent).
- `POST /v1/tls/intermediate`, body `{"deployment": "<name>", "public_key": "<base64 DER
  SubjectPublicKeyInfo, P-256>"}` (at most 64 KiB) → `200` `{"certificate": "<PEM>"}`, the
  intermediate of §5 for that key signed by the deployment's root. `401` when the token is
  missing, invalid, expired or for another audience; `403` when the pod name does not match
  the deployment or has no bound-pod claim; `400` for a malformed body or key; `413` over the
  limit; `405` for another method; `503` when the seed is not loaded, the `TokenReview`
  fails or the `TokenReview` budget is spent. The posted key must be a point of the P-256
  curve (`400` otherwise). `TokenReview` calls are limited per source IP of the connection (burst 10, 2 per
  second, whatever the answer; at most 4096 sources are tracked, the oldest is forgotten first)
  and by a global ceiling (burst 200, 100 per second), so that callers with arbitrary tokens
  neither flood the API server nor, from one pod, starve the sidecars of the others; the HKDF and ECDSA work runs outside the async threads that
  serve the webhook.
- `POST /v1/workloads/<deployment>/report` (F5): the discovery push of §6 "Shipping". Headers
  `Authorization`, `Content-Type: application/json` and `Content-Length` (required); body
  `{"session": "<32 lower-case hex>", "sequence": <1 … 2^53>, "report": <§6 report>}` (from F8
  version 1 or 2, §6.1; a controller up to F7 accepts version 1 only). The
  deployment is in the path, so it is authorised before the body is read. → `200`
  `{"applied": true}` once the push is merged into the deployment's store (§7.3), or
  `{"applied": false}` when this pod and session already had this or a later sequence
  applied (a retry: nothing changes). `401`/`403` as above; `400` for a missing
  `Content-Length`, a malformed body, session or sequence, or a report that fails the
  structural check of §7.3; `413` when `Content-Length` is over
  `MIMIC_SIDECAR_API_MAX_REPORT`, answered before the body is read; `405` for another method;
  `429` over the rate; `507` when the report has more than `MIMIC_STORE_MAX_PUSH_ENDPOINTS`
  endpoints in a direction or, from F8, more than `MIMIC_STORE_MAX_AUTHORITIES` outbound scopes
  (the push is refused whole, never truncated) or when the
  deployment is new and its namespace already holds `MIMIC_STORE_MAX_DEPLOYMENTS`
  deployments; `503` with `Retry-After: 1` when the bodies being
  read or still being stored would exceed `MIMIC_SIDECAR_API_BODY_BUDGET`, Valkey is
  unavailable, the write lost 5 optimistic attempts (§7.3) or the `TokenReview` fails. A push
  holds a share of the budget equal to its `Content-Length` from before its body is read until
  its store write ends (F8, SHR-18): the parsed reports of a write cost about three times the
  body, so two pushes near `MIMIC_SIDECAR_API_MAX_REPORT` are stored one after the other (the
  second gets the `503` and the sidecar re-posts it), while small pushes stay concurrent.
  From #339 a parsed report costs at most 1.6 times its JSON (SHR-22) and a write holds one
  parsed report, so two such pushes fit in memory at once (SHR-23); the default budget is
  unchanged.
- `GET /v1/workloads/<deployment>/egress?format=1` (F9): the deployment's egress set, with
  `ETag` and `304`, under the memory bounds of §7.7. It shares the admission, authentication
  and `TokenReview` limits above; it has no body.
- **Shipping client** (sidecar, F5). Every `MIMIC_DISCOVERY_PUSH_INTERVAL` (the first one after
  a random delay within one interval, so that the pods of a rollout do not push together),
  the sidecar freezes the report recorded since its last acknowledged push into a *pending*
  report with the next sequence (nothing is frozen when it covers no exchange and, from F8, no
  outbound tunnel) and posts it;
  new exchanges go into a fresh *current* report. The session is 128 random bits drawn at
  process start, so a restarted sidecar starts a new session at sequence 1. A pending report
  is retried unchanged (same body) until it is settled: `200` settles it; `413` removes its
  samples and posts it again with the same sequence (it was not applied); `400`, `405` and
  `507` drop it with one WARN `report rejected` (`status`); anything else (connection error,
  no answer within 30 s, `401`, `403`, `429`, `5xx`) retries with the backoff of §5 (1, 2,
  4 … 60 s, or `Retry-After` when longer, at most 300 s). A `200` whose body is not a JSON
  object with a boolean `applied` counts as a failed attempt (`error`), not as an
  acknowledgement. A settled report (acknowledged or dropped) ends the failure streak: the
  backoff restarts at 1 s and the next failure logs its WARN again. No new report is frozen
  while one is pending.
  **Fallback to inbound only** (F8). A `400` or `507` answer to a pending report that holds an
  outbound scope does not drop it. When every earlier attempt of that sequence got an answer
  (`400`, `413`, `429`, `503` or `507`: none of them applies anything), the sidecar moves the
  outbound part of the pending report back into the current report (merged within the sidecar
  bounds of §6.1, so it is shipped by a later push). When an earlier attempt had an unknown
  outcome (connection error after the body was sent, no answer within 30 s), a replica may have
  applied it while a mixed or rolled-back set of replicas answers the retry `400`: the outbound
  part is then dropped, never moved back, so that no outbound exchange is counted twice; that
  loss is counted (`mimic_discovery_outbound_dropped_total`) and logged once per sequence (WARN
  `outbound not shipped`, `reason` = `unknown_outcome`). In both cases the sidecar posts, with the same session and sequence, the remaining inbound part
  (`"version": 1`, the body an F7 sidecar would have sent), which then follows the rules above
  (a `400` or `507` to it drops it with WARN `report rejected`). When the inbound part covers no
  exchange nothing is posted and the sequence is used again by the next freeze (not after an
  unknown outcome: a replica may have applied that sequence and would answer `applied: false`
  to other data, so the next freeze takes the next sequence). The next freeze
  tries version 2 again. The inbound part of a report that a replica already applied is a
  duplicate of its pod, session and sequence (`applied: false`), so it is never applied twice
  either; this is the only case, with `413`, where a pending
  sequence is posted with another body. This keeps the inbound data whatever the controller
  version: a controller up to F7 (`400` with an empty body), a mixed set of replicas during a
  controller rollout, a controller rollback, and an outbound part that a controller refuses
  (more authorities than its `MIMIC_STORE_MAX_AUTHORITIES`, a bound a later version adds).
  The attempt is counted `inbound_only` in `mimic_discovery_pushes_total`, with one WARN
  `outbound not shipped` (`status`) per streak of fallbacks, the streak ended by an acknowledged
  version 2 push. An attempt has an unknown outcome when it got no answer after the body was
  sent (not when the connection was refused), or `500`, `502`, `504`, any status outside
  `200`, `400`, `401`, `403`, `405`, `413`, `429`, `503`, `507`, or a `200` that is not a JSON
  object with a boolean `applied`; the flag lasts for the whole sequence, whatever later
  attempts are answered, and the next sequence starts without it. When the per-streak and the
  per-sequence case both apply (a fallback after an unknown outcome), only the per-sequence
  line is written (it carries `status` and `reason`) and it also counts as the streak's line;
  it is written even in a streak that has already written its own. The flush at `SIGTERM`
  applies the fallback too (within its deadline), posts the remaining inbound part and does not
  try version 2 again.
  *Rollout order*: the chart upgrades the controller; a sidecar changes only when its pod is
  recreated (the webhook sets the image at admission), so F8 sidecars meet F8 controllers in the
  normal order, and the fallback covers the window where both versions answer, and rollbacks.
  Pending and current samples share `MIMIC_DISCOVERY_SAMPLE_BUDGET`. At `SIGTERM`, once the
  proxy has drained (at most three quarters of `MIMIC_SHUTDOWN_GRACE`, so that the flush has
  at least a quarter of it), the sidecar posts the pending then the current report within what
  remains of `MIMIC_SHUTDOWN_GRACE`; what is not acknowledged then is lost, with one WARN
  `report not shipped` (`exchanges`). The kubelet stops a sidecar after the app containers
  and leaves it only what is left of the pod's grace period, at least 2 s (an app that
  ignores `SIGTERM` uses the whole period, and the README asks apps to exit on it), so the
  flush does not back off: a failed post is repeated after a fixed 200 ms, whatever the backoff
  or the `Retry-After`, until the deadline. Without `MIMIC_CONTROLLER_URL` nothing is shipped (one
  INFO `report shipping disabled` at start) and the report accumulates as in F4. `GET
  /discovery` serves the current report (from F7 with the outbound part, shipped from F8, §6.1). Logs (`target` = `discovery`): DEBUG `report pushed`
  (`sequence`, `exchanges`, `bytes`, `applied`), WARN `report push failed` (`status` or
  `error`, `attempt`) once per failure streak, INFO `report push recovered`; never the token.
- One log line per request (`target` = `sidecar_api`): `endpoint` (the route, e.g.
  `/v1/workloads/{deployment}/report`), `namespace`, `pod`, `deployment`, `result` (`ok`,
  `duplicate`, `unauthenticated`, `forbidden`, `invalid`, `too_large`, `rate_limited`,
  `push_limit`, `deployment_limit`, `unavailable`, from F9 `not_modified` and `egress_limit`),
  plus for an egress pull (F9) `bytes` (the body sent, 0 for `304`), for a push `session`, `sequence`, `bytes`,
  from F8 `version` (the pushed report's, when it parsed), and for
  `invalid` a `reason` from a fixed list, and, when the request failed on the controller's
  side, `error`; a request writes no other line; never the token, the key or report content.
  The sidecar reads every answer with a 64 KiB cap while it streams. Metric
  `mimic_sidecar_api_requests_total{endpoint,result}`. A rate-limited request is logged at
  DEBUG only (the metric counts it), so a flood cannot flood the logs.

### 7.3 Discovery store (F5)

- Valkey ≥ 9.0 (hash field expiry), standalone (cluster mode is not supported). The
  controller's `store` module is the only code that reads or writes these keys. `<ns>` and
  `<dep>` are the Kubernetes names (DNS-1123: no `:`, `/`, `{` or `}`), checked before a key
  is built:

  | Key | Type | Content | Expiry |
  |---|---|---|---|
  | `mimic:v1:{<ns>/<dep>}:report` | string | merged report: canonical §6 JSON, `pod` null, with `last_seen`; from F8 its inbound part only, always `"version": 1` | `MIMIC_STORE_TTL` after the last applied push |
  | `mimic:v1:{<ns>/<dep>}:outbound` | string | (F8) merged outbound part: canonical §6.1 JSON, `"version": 2`, outbound scopes only, `pod` null, with `last_seen`; absent while the deployment has no outbound scope | same |
  | `mimic:v1:{<ns>/<dep>}:meta` | hash | `version` (write counter), `updated` (ms of the last applied push), `exchanges`, `endpoints_inbound`, `endpoints_outbound`, `sample_bytes`, `report_bytes` (from F8 of `:report` and `:outbound` together), from F9 `outbound_digest` (`<version>:<hex SHA-256 of :outbound>`, §7.7) | same |
  | `mimic:v1:{<ns>/<dep>}:pods` | hash | field `<pod>` → ms of its last push (applied or not) | each field min(24 h, `MIMIC_STORE_TTL`) after its last write |
  | `mimic:v1:{<ns>/<dep>}:seq` | hash | field `<pod>/<session>` → last applied sequence | same as `:pods` fields |
  | `mimic:v1:{<ns>/<dep>}:pulls` | hash | (F10) field `<pod>` → `<mode> <ms>`: the mode the last `200` or `304` egress answer to that pod carried and when (§7.7) | same as `:pods` fields; the key itself `MIMIC_STORE_TTL` after its last write |
  | `mimic:v1:ns:<ns>` | sorted set | member `<dep>`, score = `updated` | `MIMIC_STORE_TTL` after the last write; members older than it removed by each write |
  | `mimic:v1:{<ns>/<dep>}:load:<uid>` | hash | (F11) a load test run (§7.8): `spec` (the `MimicLoadTest` name and spec, no Secret value), `stop` (`0`/`1`), `start` and `end` (ms), `phase`, `reason`; while active `plan`, `ticket` and per generator worker index `<i>` → its last cumulative results (at most 256 KiB each); once ended only `summary` (at most 192 KiB, enforced by the end script) and `pods` (the number of pod fields the end script found) besides the small fields; ended runs together hold at most deployments × 20 × 192 KiB at the defaults (each expires `MIMIC_STORE_TTL` after its last write) | `MIMIC_STORE_TTL` after its last write |
  | `mimic:v1:{<ns>/<dep>}:loads` | sorted set | (F11) member `<uid>`, score = start ms; at most 20, the oldest ended run (and its `:load:<uid>`) removed by each admission | `MIMIC_STORE_TTL` after the last write |
  | `mimic:v1:loads:active` | sorted set | (F11) member `<ns>/<dep>/<uid>`, score = the run's deadline (ms: start + `activeDeadlineSeconds`); the admission script removes members past their score, then refuses a member of a deployment already present (`Busy`) or past `MIMIC_LOAD_MAX_RUNS` members (`ClusterBusy`); the end of a run removes it | none (members expire by score) |
  | `mimic:v1:loads:reserved` | hash | (F11) field `<ns>/<dep>/<uid>` → bytes reserved by an active run (§7.8 "Admission"); same lifetime as its `:active` member (the admission script drops fields whose member is gone). The admission script reads `loads:active` and `loads:reserved` together with a deployment's keys, which are not in one hash slot: it needs the standalone Valkey of §3 (cluster mode is not supported) | none |
  | `mimic:v1:workloads` | sorted set | member `<ns>/<dep>`, score = `updated` | `MIMIC_STORE_TTL` after the last write; members older than it removed by each write |

- **Push processing**: structural check of the posted report, then re-redaction, then
  stamping, then the write. Nothing of a push is stored or logged before its re-redaction.
  - *Structural check* (pure, `mimic-core`): `"version": 1` (from F8, the version rule of §6.1:
    1 without an outbound scope, 2 with one; reason `version`) and every bound of §6 — at most
    `MIMIC_STORE_MAX_PUSH_ENDPOINTS` endpoints per direction (else `507`, reason
    `push_limit`, so that one pod cannot fill the store in one push), 20 samples and 16 hosts (each at most
    1024 bytes) per endpoint, statuses 100–599, histogram buckets within the §6 range, at most
    512 nodes, 16 levels and a cut level of at most 16 per schema, names of at most 128
    bytes, at most 64 query keys and 64 headers, 128 volatile fields and 32 hashes per
    sketch, paths and templates of at most 2048 bytes and 32 segments, 16 media types per
    side, sample bodies only for parsed media types and of at most 256 KiB, no unknown field.
    A template's length counts each literal segment's bytes plus its `/`, and a parameter as
    its `/` alone (the rendered `{name}` can be longer than the raw path); the `{}` that marks
    a schema's cut level is not a level.
    A sample's `bytes` (what `MIMIC_STORE_SAMPLE_BUDGET` counts) is taken from the wire and
    verified: it must equal the serialised size of the sample with `bytes` and `head_ms` at 0
    (reason `sample_bytes`), so a forged value cannot hide samples from the budget; byte sums
    saturate.
    From F8 an outbound scope also has: a key of the §6.1 authority grammar (reason
    `authority`); at most `MIMIC_STORE_MAX_AUTHORITIES` outbound scopes in the push (else `507`,
    reason `push_limit`); an `origin` with schemes `http` and `https` only, at most 16 `sni`,
    each a host of the §5 SNI rule (lower-case DNS name, at most 253 bytes), and at most 16
    `dst`, each an IP literal and port of the §6.1 grammar (reason `origin`); `tunnels` kinds of
    the §6.1 list only (reason `tunnels`); outbound endpoints without `hosts` (reason `hosts`).
    `origin` or `tunnels` on an inbound scope is `malformed`.
    Any other violation is `400` with its `reason`.
  - *Re-redaction* (pure, idempotent): the §6 redaction applied again to every sample, host,
    concrete path, literal path segment (tree keys and gRPC paths; keys that become equal
    merge), schema and parameter key and volatile field with the built-in rules and the union of
    `spec.discovery.redact` of the `MimicWorkload`s naming the deployment (from the
    controller's cache, so a name added after the pod started applies); a field that becomes
    redacted loses its sketch (but for the hash of the marker, the one a sidecar that knew the name records, so that both are equal) and is marked `redacted`, a sample that changes is keyed by the
    hash of its new content. Names are redacted as the sidecar does: a key whose name is
    redacted has the schema of the marker (a string without format), and the volatile fields
    below a redacted name become that name's field; a name is never redacted by value. A
    volatile sketch holds hashes only, so a detected form (JWT, card, token prefix) cannot be
    found in it: its sketch is dropped when a retained sample shows a detected form at that
    field, and kept otherwise (§11.1). From F8 an outbound exchange is re-redacted with the
    outbound view of §6.1 as well (the request header allowlist, `key` and `sig` by name), and
    each outbound scope's identity: the host of an authority key that a value detector catches
    becomes the marker (`<redacted>:<port>`; scopes that become equal merge, trees, `origin` and
    `tunnels` included, as path keys do), and an `origin.sni` value that a detector catches is
    removed (the sidecar never records one). A sidecar of an older version, or a faulty one,
    cannot put a value into Valkey that the controller's rules redact.
  - *Stamping*: every endpoint and sample of the push gets `last_seen`, the current UTC day
    (days since 1970-01-01), and from F8 so does every outbound scope of the push (its `origin`
    and `tunnels` are dated by it); a merge keeps the larger value. Sidecar reports carry no
    `last_seen`.
  - *Write*: within a replica, one writer per deployment; pushes that arrive while it writes
    are merged into its next write (group commit), and each is answered after the write that
    holds it. At most 2 deployments are written at once per replica (memory bound). A write
    reads `:report` (from F8 with `:outbound`: the stored report is their union), `:meta`
    `version` and the `:seq` fields of its pushes; answers
    `applied: false` for a push whose sequence is not above the stored one of its pod and
    session; merges the others into the stored report with the **bounded merge** below
    (from #339 a push waits for its write as its checked JSON, re-redacted and stamped, not
    as a parsed report, and the merge reads it into the stored report as it parses it: no
    second parsed report is built, and a write that starts again reads it again; the result
    is byte-identical to the bounded merge of the parsed push, SHR-21);
    drops endpoints and samples whose `last_seen` is older than `MIMIC_STORE_TTL` (in whole
    days), and from F8 the outbound scopes that are older and hold no endpoint any more; and
    stages the two parts under temporary keys `mimic:v1:{<ns>/<dep>}:stage:<128-bit random,
    hex>:report` and `…:outbound` (1 MiB per command: `SET`, then `APPEND`s, so that no command
    holds a copy of a whole part, #339; every command also renews the 60 s expiry of both keys,
    so that no stage key exists without one, and a part that waits for the other does not
    expire), and runs one Lua script that, only if `version` and the `:seq`
    fields are still those read and the staged keys have the lengths announced, `RENAME`s the
    staged keys over `:report` (from F8 the inbound part, and the outbound part over
    `:outbound`, deleted when there is none; so a reader sees the old value or the new one, never
    a part of it, and a crash between two chunks leaves only a temporary key that expires, which
    no read uses; every refusal of the script, and every error or cancellation of the writer
    between the first chunk and the script's answer, deletes them, best effort: the expiry
    bounds what a crash leaves; a write holds up to about 3 times the size of a report in
    Valkey, the old value, the staged parts and the new value, which `maxmemory` sizing must
    allow), writes `:meta` (`version` + 1), `:seq`, `:pods` and both indexes,
    refreshes the expiries and prunes the indexes, refusing a new deployment past
    `MIMIC_STORE_MAX_DEPLOYMENTS` in the namespace (`507`). Otherwise nothing is written and
    the write starts again (at most 5 attempts, 10–100 ms random backoff, then `503`). So a
    push is applied once, whichever replica receives it and however often it is retried
    (within the `:seq` field expiry, §11.1). A stored value that does not parse or fails the
    structural check (a `:report` that is not version 1 or an `:outbound` that holds an inbound
    scope included) is never overwritten: ERROR `stored report unreadable` (`namespace`,
    `deployment`, from F8 `key`: `report` or `outbound`), push `503`, read `500`. Keeping
    the outbound part in its own key leaves `:report` readable by a controller up to F7: a
    rollback keeps serving and merging the inbound data and ignores `:outbound`, which expires
    unless an F8 controller writes again (§11.1).
  - *Failures*: the connection to Valkey is shared by all writers. A Valkey reply error (`OOM`
    under `noeviction`, a script error, `WRONGTYPE`) answers the pushes of the write `503` and
    keeps the connection; only a connection error (I/O, timeout, closed, refused) drops it, so
    that the next attempt makes a new one. The WARN for a failed write (`valkey command failed`,
    `valkey connection failed`) is logged once per failure streak: for the first failed write
    after a write that succeeded, not for each push that follows. A writer that panics logs one
    ERROR `store writer failed` (`namespace`, `deployment`), drops its queue entry and answers
    `503` to the pushes of its batch and to those waiting; the next push for the deployment
    starts a new writer, so no push waits for a writer that is gone.
- **Bounded merge** (pure, `mimic-core`): the §6 merge under the store limits
  (`MIMIC_STORE_MAX_LITERALS`, `MIMIC_STORE_MAX_ENDPOINTS` per direction,
  `MIMIC_STORE_SAMPLE_BUDGET` per deployment, from F8 `MIMIC_STORE_MAX_AUTHORITIES`);
  counters saturate at 2^64 − 1. The merge
  re-applies templating on the union: a node that the deployment sees above the literal limit
  absorbs. Below the limits the stored report does not depend on the order or grouping of
  pushes. At a limit, what is kept is chosen by traffic, never by key order alone, so that
  endpoints that sort first (`/!`, `/0`, the paths of a scanner the app answers `2xx`) cannot
  crowd out the deployment's real API:
  - *Authorities* (F8), applied first: when the union holds more than
    `MIMIC_STORE_MAX_AUTHORITIES` outbound scopes, those kept are the first ranked by traffic
    (the exchanges of their endpoints plus their `tunnels` counts) descending, then stored
    before new, then by key; a scope that is not kept goes with its endpoints, samples,
    `origin` and `tunnels` (`evicted` kind `authority` when it was stored, refused
    `authority_limit` when it was new; its endpoints and samples are not counted again).
  - *Endpoints*: when the union holds more than `MIMIC_STORE_MAX_ENDPOINTS` endpoints in a
    direction, the endpoints kept are the first ones ranked by total exchange count (the sum
    of their status counters) descending, then stored before new (an endpoint of the stored
    report wins a tie over one this write adds), then by key (method, template). The others
    are dropped (`evicted` when they were stored, `endpoint_limit` refused when they were new).
  - *Samples*: a new sample that does not fit `MIMIC_STORE_SAMPLE_BUDGET` is refused
    (`budget`), unless its endpoint has no sample of its status class (`1xx` … `5xx`): then
    samples are evicted to make room, one at a time, each time from the (endpoint, status
    class) of the deployment holding the most samples (ties: the most sample bytes, then the
    larger key), the sample with the largest key first, never taking a class of an endpoint
    to zero samples; if room cannot be made that way the new sample is refused (`budget`)
    and nothing is evicted. *Per-direction floor* (F8): the (endpoint, status class) pairs
    that can lose a sample are those of the new sample's direction, and those of the other
    direction only while that direction holds more than half of `MIMIC_STORE_SAMPLE_BUDGET`,
    so a direction holding at most half of the budget never loses a sample to the other; a
    direction may use more than half while the other leaves it free (the store has no fixed
    split, unlike the sidecar's outbound half, §6.1). Counters, histograms, schemas and volatility merge whatever is
    refused or evicted.
  Ties left by those rules are broken by the full endpoint key. A stored report already over
  the budget (a lowered `MIMIC_STORE_SAMPLE_BUDGET`) is trimmed by the same eviction rule
  (from F8 from the direction that holds more than half of the budget, while it does); a
  sample added and evicted within one merge counts as `evicted`.
  Refusals are counted in `mimic_store_refused_total{kind,reason}` with one WARN
  `store limit reached` (`namespace`, `deployment`, `reason`) per deployment and reason per
  process; evictions in `mimic_store_evicted_total{kind}` with one WARN `store evicted`
  (`namespace`, `deployment`, `kind`, `count`) per deployment and kind per process.
- **Outbound (F7, F8).** Up to F7 the structural check accepts `"version": 1` only, and
  sidecars push no outbound data (§6.1): the store, the status and the UI hold inbound
  endpoints only. From F8 the store accepts version 2 pushes with the outbound check,
  re-redaction, stamping, authority limit and sample floor above, and keeps the merged
  outbound part in `:outbound`; pushes of F5–F7 sidecars (version 1) are processed exactly as
  before, and a deployment without outbound data has the same keys and bytes as in F7.
- **Split (F8).** The split of the merged report into `:report` and `:outbound` gives the
  inbound part `exchanges − outbound_exchanges` and the outbound part `outbound_exchanges`
  (§6.1), exactly, also after authorities or endpoints were evicted or expired: their
  exchanges stay counted on the outbound side and never turn into inbound ones, as long as
  one outbound scope remains. When the last one goes, the report drops to version 1 and its
  outbound exchanges leave with it (§6.1), so none is counted as inbound. The inbound part
  never carries the counter (it is version 1); the union of the parts adds them back.
- **Pod churn**: no pod is read. A deleted pod's data stays merged (the deployment's API is
  the union over its life) and it ships its last report at `SIGTERM`; the pods of a rolling
  update push into the same key; a deleted pod's `:pods` and `:seq` fields expire after
  min(24 h, `MIMIC_STORE_TTL`). A deployment's keys outlive it until `MIMIC_STORE_TTL`, and a
  deployment created again with the same name continues them.
- **Valkey memory**: the chart runs Valkey with `maxmemory` (75 % of its memory limit) and
  `maxmemory-policy noeviction`: a full Valkey refuses writes (pushes get `503`, sidecars
  keep merging locally within their bounds) instead of evicting one key of a deployment and
  breaking its consistency. Chart defaults: Valkey memory limit `1Gi`, memory request `768Mi`
  (equal to `maxmemory`: the scheduler reserves what a full store uses, which is not then the
  first pod evicted under node memory pressure; the 256 Mi between request and limit is the
  headroom of AOF rewrites) and PVC `4Gi` (AOF rewrites need about twice the data), controller memory limit `768Mi` (the pushes the body budget
  lets through at once, each held until its write ends; from F8 a report holds up to
  `MIMIC_STORE_MAX_ENDPOINTS` endpoints in each direction, the sample budget staying per
  deployment, and SHR-18 checks that the limit still holds). The chart sets
  `MALLOC_ARENA_MAX=2` on the controller to bound the allocator's arenas. An inbound document
  parses only `:report` and an outbound document only `:outbound`; the index and the endpoint
  list read both.

### 7.4 `MimicWorkload` status (F5)

- The CRD has a `status` subresource. Every `MIMIC_STATUS_INTERVAL` each controller replica
  computes, for each `MimicWorkload` of its cache, from the `:meta` and `:pods` keys of its
  deployment (a pure function of them, the window and the current time):

  ```yaml
  status:
    discovery:
      endpoints: {inbound: 12, outbound: 3}  # per direction; directions without any omitted
      totalEndpoints: 15                     # (F8) sum of the directions, 0 without data
      exchanges: 34567
      sampleBytes: 1048576
      pods: 2                                # pods whose last push is within MIMIC_STATUS_POD_WINDOW
      lastPushTime: "2026-11-01T10:00:00Z"   # last applied push, UTC, seconds; absent without data
  ```

  and applies it by server-side apply on the status subresource (field manager
  `mimic-controller`, forced: the controller owns `status.discovery` alone, so a conflict
  cannot arise, and a refused apply is never repeated as a conflict at every interval) only
  when it differs from the status in its cache. An apply that takes more than 10 s is
  abandoned, counted as an error and logged as a failed apply; the next pass retries it.
  An unreadable `:meta` or `:pods` is logged once (ERROR, §7.5) for as long as it stays
  unreadable, not at every pass, and the status of that deployment is left as it is. Both replicas
  compute the same value except at a window edge, and an apply of an equal value changes
  nothing, so no leader is elected. A deployment without data gets zero counts and no
  `lastPushTime`. While Valkey is unavailable the status is left as it is (one WARN
  `status not updated` until it recovers). `kubectl get mimicworkload` prints `Mode`,
  `Endpoints`, `Pods` and `Last push` (an age, as kubectl prints `date` columns). Up to F7
  `Endpoints` was the inbound count; a printer column cannot add fields, so from F8 the
  controller writes `totalEndpoints` (inbound + outbound) and `Endpoints` reads it. The CRD
  schema defaults `endpoints.inbound` and `totalEndpoints` to `0`, so the column is `0` and not
  blank without endpoints (and for a status written by a controller up to F7 the column reads
  `0` until an F8 controller applies one), while the stored status omits the default
  `endpoints.inbound` and the controller compares statuses with the defaults applied.
  `exchanges` and `sampleBytes` count both directions.
- **Mode convergence (F10).** The same pass also reads `:pulls` and writes

  ```yaml
  status:
    mode:
      mode: mock        # effective mode of the deployment (§7), from the controller's cache
      pods: 2           # pods whose last egress answer is within MIMIC_STATUS_POD_WINDOW and carried `mode`
      otherPods: 1      # pods within the window whose last answer carried the other mode
  ```

  in the same apply (`status.mode` is owned by the controller like `status.discovery`; both
  counts default to `0`). A load test can start when `otherPods` is `0` and `pods` equals the
  deployment's ready replicas. `kubectl get mimicworkload` also prints `Mode pods`
  (`.status.mode.pods`). An unreadable `:pulls` field is skipped (not counted) and logged once,
  as `:pods`. Pods that never pull (no `MIMIC_CONTROLLER_URL`) are in neither count.

### 7.5 Read API and UI (F5, F6)

- Plain HTTP on `MIMIC_UI_ADDR` (default `127.0.0.1:8080`, loopback: reached only through
  `kubectl port-forward` to a controller pod); a peer that is not a loopback address gets
  `403` whatever the bind address, and so does a request whose `Host` is missing or is not
  `localhost`, `127.0.0.1` or `[::1]` (with or without a port), so that a web page cannot
  reach an open port-forward through DNS rebinding; an empty port (`127.0.0.1:`) is not a port
  and gets `403` as well. `GET` and, from F6, `HEAD` (RFC 9110: the same status and headers as
  `GET`, no body); every other method gets `405` with `Allow: GET, HEAD`. In F5 `HEAD` got `405`
  (`Allow: GET`); F6 serves it because browsers and link checkers expect it wherever `GET` is
  served (#293 note 28). JSON, sorted keys:
  - `GET /v1/workloads` → `{"workloads": [{"namespace", "deployment", "endpoints",
    "exchanges", "sample_bytes", "last_push"}]}`, sorted by namespace then deployment
    (`endpoints` counts both directions);
  - `GET /v1/workloads/<ns>/<dep>` → the same object plus `"pods": {"<pod>": "<last push,
    RFC 3339>"}`;
  - `GET /v1/workloads/<ns>/<dep>/report` → the stored `:report` bytes; from F8, when
    `:outbound` exists, the canonical serialisation of the union of both (`"version": 2`,
    rendered like the routes below), so the route always answers the deployment's whole
    merged report;
  - `GET /v1/workloads/<ns>/<dep>/endpoints` (F6) → `{"endpoints": [{"direction", "port",
    "method", "template", "grpc", "exchanges", "statuses": {"<code>": <count>}, "latency_ms":
    {"p50", "p90", "p99"}, "hosts", "last_seen", "document"}]}` in the order of the report's
    endpoints (direction, port, template, method; from F8 outbound entries follow the inbound
    ones, ordered by authority, template, method, with `"direction": "outbound"`, `authority`
    instead of `port` and no `hosts`); `latency_ms` values are the §7.6 quantiles,
    `last_seen` is `YYYY-MM-DD` or absent, `document` is the id of the §7.6 document that holds
    the endpoint;
  - `GET /v1/workloads/<ns>/<dep>/openapi` (F6) → `{"documents": [{"id", "direction", "port",
    "endpoints"}]}`, one entry per document of §7.6, sorted by id (from F8 an outbound entry has
    `authority` instead of `port`, and `tunnels`, the sum of its scope's tunnel counts);
  - `GET /v1/workloads/<ns>/<dep>/openapi/<id>` (F6) → the OpenAPI 3.1 document `<id>` (§7.6),
    `application/json`; `400` for an id outside the grammar of §7.6, `404` for a well-formed id
    that the report does not hold. From F8 the `<id>` path segment (here and in the page route
    below) is percent-decoded once (RFC 3986) before it is matched against the grammar, so an
    outbound id is written with its `[`, `]`, `<` and `>` percent-encoded and `:` raw or encoded;
    a segment that does not decode (bad `%` escape, not UTF-8) is `400`, and `mimic.js` builds
    links with `encodeURIComponent`.

  `404` for a deployment without data, `400` for a name that is not DNS-1123, `503` while
  Valkey is unavailable. A timestamp beyond year 9999 is shown as `9999-12-31T23:59:59.999Z`.
  No write and no delete: data leaves by expiry (§7.3).
  A stored value that does not parse is `500` with one ERROR line per read, with the fields
  `namespace` and `deployment`: `stored report unreadable` for `:report` (§7.3), `stored meta
  unreadable` for `:meta` and `stored pods unreadable` for `:pods`. In the list, an entry whose `:meta` is unreadable is
  skipped, with that one ERROR line, instead of failing the whole list.
  The `endpoints`, `openapi` and document routes (from F8 also `report` when `:outbound`
  exists) parse the stored report (`Report::from_json`
  and the structural check of §7.3, as a push; from F8 the union of `:report` and `:outbound`) and render it on a blocking thread, at most 2 at
  once per replica (others wait), so a large report cannot stall the listener; rendering is a
  pure function of the report and the `:meta` `version` and cannot fail on a checked report (a
  serialisation error is `500` with ERROR `openapi render failed`, `namespace`, `deployment`).
- **UI pages (F6).** Static HTML, the same bytes for every deployment, filled in the browser by
  one script, `mimic.js`, that reads the JSON routes above and writes text through DOM APIs only
  (`textContent`, `createElement`; never `innerHTML`, never `eval`):
  - `GET /` → `308` to `/ui/`;
  - `GET /ui/` → the workloads list: namespace, deployment, endpoints, exchanges, last push,
    each linking to its deployment page;
  - `GET /ui/workloads/<ns>/<dep>` → the deployment page: the summary and pods of
    `/v1/workloads/<ns>/<dep>`, the endpoint list of `/endpoints` (direction, port or, from F8,
    authority, method, template, exchanges, statuses, latency quantiles, last seen), and one
    link per document (from F8 with each outbound document's tunnel count);
  - `GET /ui/workloads/<ns>/<dep>/openapi/<id>` → Swagger UI showing that document, with a
    link to the raw JSON (for import into other tools).

  `400` for a name or id outside its grammar (no page is served for it). A page of a
  deployment without data is served and shows "no data" from the `404` of its JSON route.
- **Swagger UI configuration** (in `mimic.js`, nothing inline): `url` = the document route;
  `supportedSubmitMethods: []` (no "Try it out" button, so the UI never sends a request to a
  workload); `tryItOutEnabled: false`; `validatorUrl: null` (no call to
  `validator.swagger.io`); `queryConfigEnabled: false` (a URL cannot load another document or
  configuration); `withCredentials: false`; `showExtensions: true` (the `x-mimic-*` fields of
  §7.6 are shown); `deepLinking: true`; base layout, no standalone preset (no "Explore" URL
  box); a `requestInterceptor` that refuses every URL other than a same-origin
  `/v1/workloads/` route (a `$ref` in a document cannot make the UI fetch anything else).
- **Static assets (F6)**, embedded in the controller binary (`include_bytes!`, no file in the
  image, no CDN, nothing fetched at run time) and served under `/ui/assets/`: `mimic.js`,
  `mimic.css`, and `swagger-ui/<version>/` with `swagger-ui-bundle.js`, `swagger-ui.css`,
  `LICENSE`, `NOTICE` and `swagger-ui-bundle.js.LICENSE.txt` of the npm package
  `swagger-ui-dist` (Apache-2.0; its licence and notices are served and kept in the source
  tree). Any other asset path is `404`. Content types are fixed per extension (`text/html;
  charset=utf-8`, `text/javascript; charset=utf-8`, `text/css; charset=utf-8`, `text/plain;
  charset=utf-8`). Source maps are not vendored.
  - *Pinning*: `crates/mimic-controller/ui/package.json` names `swagger-ui-dist` at an exact
    version and `package-lock.json` holds its tarball URL and `sha512` integrity; Dependabot
    (`npm` ecosystem, 14-day cooldown) proposes updates; `scripts/check-cooldown.py` also reads
    that lockfile (publication time from the npm registry) with the same 14 days and allowlist.
  - *Vendoring and integrity*: `scripts/vendor-swagger-ui.py` downloads the locked tarball from
    `registry.npmjs.org`, verifies its `sha512` against the lockfile, extracts the files above
    into `crates/mimic-controller/ui/vendor/swagger-ui/` and writes `SHA256SUMS`; `--check`
    (CI supply-chain stage) does the same into a temporary directory and fails unless the
    committed files are byte-identical. A unit test of the controller checks the embedded bytes
    against `SHA256SUMS`. A Dependabot bump therefore fails CI until the files are vendored
    again by the script and the diff is reviewed.
- **Response headers (F6)**, on every answer of the listener (`403`, `404` and `405`
  included):
  - `Content-Security-Policy: default-src 'none'; script-src 'self'; style-src 'self';
    img-src 'self' data:; font-src 'self'; connect-src 'self'; base-uri 'none'; form-action
    'none'; frame-ancestors 'none'` (no inline or evaluated script; the page can only talk to
    the listener's own origin, i.e. the port-forward). A relaxation that Swagger UI needs is a
    spec change; `'unsafe-eval'` and an inline script source are never allowed;
  - `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `X-Frame-Options: DENY`,
    `Cross-Origin-Opener-Policy: same-origin`, `Cross-Origin-Resource-Policy: same-origin`;
  - `Cache-Control: no-store`, except `public, max-age=31536000, immutable` for the versioned
    `swagger-ui/<version>/` assets;
  - no `Access-Control-Allow-*` header: other origins cannot read an answer.
- **Observability (F6)**: `mimic_ui_requests_total{route,code}` (`route` is the route pattern,
  e.g. `/v1/workloads/{namespace}/{deployment}/openapi/{id}`, or `other`; `code` the status
  code) and `mimic_ui_render_duration_seconds` (histogram of parse + render, buckets as
  `mimic_store_write_duration_seconds`) and `mimic_ui_skipped_total{reason}` (list entries
  skipped because their `:meta` is unreadable, one per entry per list read). No line is written per request; no line or series
  holds a path, template or report content.
- Consumers: F8 adds outbound documents (§7.6) with no new route; F9 adds an
  authenticated read of a sidecar's own deployment on the sidecar API (§7.2) for egress
  recordings and the mode (F10); F11 reads ingress templates, samples, schemas and volatility
  through the same `store` module (§7.8) and adds the `loadtests` routes and a "Load tests"
  table on the deployment page.

### 7.6 OpenAPI mapping (F6)

A pure function in `mimic-core` (`discovery::openapi`) of a checked stored report, the
namespace, the deployment and the `:meta` `version`; no clock, no I/O. Output is
`serde_json::Value` serialised with sorted keys, arrays in the orders stated below, so the same
input gives byte-identical output. Only the report's names, structure and **samples** (redacted
by the sidecar and again by the controller, §7.3) reach the document; nothing recorded is ever
written into a `description`, `summary` or `title` (Swagger UI renders those as Markdown).

- **Documents.** One per scope of the report: `inbound-<port>` per inbound port (decimal, no
  leading zero; id grammar `inbound-[1-9][0-9]{0,4}`, port 1–65535). From F8, one per outbound
  scope, `outbound-<authority>`: the `<host>:<port>` key of §6.1 as it is (id grammar
  `outbound-` followed by the §6.1 authority grammar, `<redacted>:<port>` included; in a URL the
  id is percent-encoded, §7.5), so `outbound-api.example.com:443`,
  `outbound-10.0.0.5:8080`, `outbound-[2001:db8::1]:8443`. A scope with tunnels only has a
  document with empty `paths`, so that destinations that cannot be documented are listed.
  Adding them changes neither the inbound documents nor the routes; the index and the
  endpoint list gain entries with `direction: "outbound"` and `authority` instead of `port`.
  A deployment that listens on one port has one document. Paths of two scopes never share a
  document, so `/a` on port 8080 and `/a` on 9090 never collide.
- **Top level.** `openapi: "3.1.1"`; `info.title` = `<ns>/<dep> inbound :<port>` (Kubernetes
  names and a number only); `info.version` = the `:meta` `version` in decimal; `info.description`
  a constant text saying the document is inferred from observed traffic. `servers`: one
  `{"url": "http://<host>"}` per distinct `hosts` value of the scope's endpoints that is a §4.1
  normalised authority (not `(invalid)`, not containing the redaction marker), sorted, at most
  16 (inbound HTTPS is not discovered, so inbound is `http`); no `servers` when none qualifies.
  *Outbound document* (F8): `info.title` = `<ns>/<dep> outbound` (the authority is recorded
  data, so it is not in the title); `info.x-mimic-authority` = the authority; `servers` = one
  `{"url": "<scheme>://<authority>"}` per scheme of its `origin.schemes` (`http`, `https`, in
  that order; the port always written), none when the host is the marker; `x-mimic-origin` =
  `{"sni": [...], "dst": [...]}` (sorted, as stored) and `x-mimic-tunnels` = the `tunnels` map
  (kind → count, sorted), both at the top level and absent when empty. Operations follow the
  rules below; an outbound endpoint has no `x-mimic-hosts`.
  `components.securitySchemes` as below; no other `components`, no `$ref` anywhere.
- **Paths.** The key is the endpoint's template (§6), parameters as rendered (`{users_id}`),
  with `{` and `}` of literal segments percent-encoded (`%7B`, `%7D`) so a literal is never read
  as a parameter; `/` for the root. gRPC paths are keys as they are, with the same escaping.
  The operations of a path item are its methods: `GET`, `PUT`, `POST`, `DELETE`, `OPTIONS`,
  `HEAD`, `PATCH` and `TRACE` (exact upper case) under their lower-case keys; any other method
  under `x-mimic-methods` (object keyed by method) with the same operation object (OpenAPI 3.1
  has no field for it). Endpoints that share a path key and a method (a gRPC and an HTTP one, or
  literals that escape alike) overwrite nothing: the first in the report's endpoint order takes
  the slot, the next ones are `x-mimic-methods` entries `<METHOD>#2`, `<METHOD>#3`, ... (a
  non-standard method's first entry is `<METHOD>`).
- **Operation.**
  - `parameters`, in this order: path parameters in template order (`required: true`; schema
    `{"type": "string"}` plus `format` when every value of that position in the endpoint's
    sample paths has the same §6 string format; no sample: `{"type": "string"}`); query keys
    in byte order (`required` when the key was in every exchange of the endpoint, schema from
    §6, `array` with `items` when repeated); request headers in byte order, except
    `accept`, `content-type` and `authorization` (ignored by OpenAPI as parameters). Each
    parameter has `examples` from the samples: `{"sample-<16 hex of the sample key>": {"value":
    <the redacted value>}}`, distinct values only, the first sample key in order kept per value.
    A parameter whose §6 field is volatile has `x-mimic-volatile: true`, a redacted one
    `x-mimic-redacted: true`.
  - `security`: when samples carry `authorization: <Scheme> <redacted>`, one requirement per
    scheme, in byte order of the lower-case scheme, naming `components.securitySchemes`
    `http_<scheme>` = `{"type": "http", "scheme": "<scheme>"}` (scheme one of the registered schemes of §6, in lower case;
    any other scheme is ignored); a redacted `authorization` without a scheme gives `apiKey` in header
    `Authorization`, named `authorization`.
  - `requestBody` when the endpoint has request bodies: `content` per media type (§6, at most
    16, byte order) with `schema` (JSON: the §6 schema; form: the object schema of its fields;
    opaque: no schema) and `examples` from the samples whose request has that media type
    (JSON value, or a form as an object of field → value, a repeated field → array of values);
    `x-mimic-count` and `x-mimic-bytes` (mean size) per media type; `required: true` when the
    media counts sum to at least the endpoint's exchanges.
  - `responses`: one per status code of the endpoint's counters (string key), `description`
    the reason phrase of the IANA registry for that code, else `Status <code>`, and
    `x-mimic-count`. A response holds the `content` (media types seen in the `content-type` of
    that status's samples, each with the endpoint-wide §6 schema of that media type and the
    examples of that status's samples) and `headers` (response headers seen in that status's
    samples, except `content-type`, with the endpoint-wide schema). A status without a sample
    has a description only. The report keeps schemas per endpoint and media type, not per
    status (§11.1).
  - Extensions: `x-mimic-exchanges` (sum of the counters), `x-mimic-latency-ms` `{"p50",
    "p90", "p99"}`: for each quantile the upper bound in ms (rounded up) of the histogram bucket
    holding it, `null` for the overflow bucket, absent without data; `x-mimic-hosts` (sorted);
    `x-mimic-last-seen` (`YYYY-MM-DD`, when stamped); `x-mimic-volatile` and
    `x-mimic-redacted`, the sorted §6 field names of the endpoint that are volatile and
    redacted; `x-mimic-grpc: true` and `tags: ["<package.Service>"]` (the first path segment)
    for a gRPC path. No `operationId` (it would need disambiguation and nothing uses it).
- **Bounds.** The document is a function of a report that passed the §7.3 check, so its size
  is bounded by the store limits (at most 20 samples per endpoint, `MIMIC_STORE_SAMPLE_BUDGET`
  of sample bytes per deployment).

### 7.7 Egress endpoint (F9)

Sidecars pull their own deployment's aggregated egress from the controller and keep it in
memory, so that F10 can answer outbound calls from it. F9 changes no traffic: the snapshot is
held, measured and summarised, never used to answer a request.

- **Egress set** (pure, `mimic-core` `discovery::egress`). The *projection* of a stored report
  is what a mock needs and nothing else:

  ```json
  {"format": 1, "authorities": {"<authority>": {"endpoints": [
    {"method": "GET", "template": "/users/{users_id}", "grpc": false,
     "statuses": {"200": 12}, "latency": <§6 histogram>,
     "volatile": ["<field>"], "redacted": ["<field>"], "samples": [<§6 sample>]}]}}}
  ```

  One entry per outbound scope that holds at least one endpoint (tunnel-only scopes and the
  inbound part are left out), endpoints in report order (template, method), samples in report
  order without `last_seen`, `volatile` and `redacted` the sorted §6 field names of the
  endpoint (the lists §7.6 writes as `x-mimic-volatile` and `x-mimic-redacted`). No schema,
  volatility sketch, `origin`, `tunnels`, `hosts`, `last_seen` or `pod`: the projection holds
  no hash of a value. It is canonical JSON (sorted keys, `BTreeMap`) and a pure function of the
  outbound part, so equal `:outbound` bytes give byte-identical projections.
  `EgressSet::from_json` reads it back with `deny_unknown_fields` and checks, each failure with
  its `reason`: `format` 1 (`format`); authorities of the §6.1 grammar (`authority`); at most
  10 000 authorities and 10 000 endpoints in all (`limit`); per endpoint the §7.3 bounds that
  apply: templates of at most 2048 bytes and 32 segments, statuses 100–599, the §6 histogram
  range, at most 20 samples each passing the sample check of §7.3, at most 128 names in
  `volatile` and `redacted` together, each at most 128 bytes (`malformed`). A new format gets a
  new number; a controller keeps serving every format that a released sidecar asks for.
- **Digest** (store change, §7.3). Every write sets the `:meta` field `outbound_digest` to
  `<version>:<hex SHA-256 of the :outbound bytes written>` (of the empty string when there is no
  `:outbound`), `<version>` being the `version` the same script writes. A digest whose
  `<version>` differs from `:meta` `version`, or an absent one, was left by a controller up to
  F8 (which writes `:outbound` without it) and is *stale*: no `304` is possible, and the pull
  goes through a build (below), which hashes the `:outbound` bytes it reads under the same
  body-budget share. The `ETag` of an answer is `"<format>-<hex digest>"`. The digest decides: a corruption of `:outbound` under an unchanged digest is not detected, neither for a `304` nor for a cached projection.
- **Route**: `GET /v1/workloads/<deployment>/egress?format=1` on the sidecar API (§7.2), with
  the admission, authentication and authorisation of §7.2 (deployment in the path, namespace
  from the token: a pod reads its own deployment only); there is no body to read. The
  controller then reads `:meta` `version` and `outbound_digest`:
  - an `If-None-Match` equal to the current `ETag` (digest not stale) answers `304` with the
    `ETag` and no body, without reading `:outbound`;
  - otherwise the projection for that digest is served from the replica's cache, keyed by
    namespace, deployment, format and digest (`<ns>/<dep>`, never the deployment name alone),
    or built: `GET :outbound`, `Report::from_json` and the §7.3 structural check, projection and
    serialisation, on a blocking thread; the cache key and the `ETag` come from the bytes
    actually read, so a write between the two reads only makes the answer newer;
  - `200` `application/json`, `ETag`, `Cache-Control: no-store`, the projection. A deployment
    without data or without `:outbound` answers `200` `{"authorities": {}, "format": 1}` (not
    `404`: the sidecar needs no special case).
  - (F10) every `200` and `304` carries the deployment's effective mode (§7) from the
    controller's `MimicWorkload` cache, read when the answer is written: `Mimic-Mode:
    discovery|mock` and, in mock, `Mimic-Latency-Factor: <decimal>` (shortest form that reads
    back to the same value, e.g. `1`, `0.5`). The mode is not part of the `ETag` nor of the
    cached projection: the projection stays a pure function of `:outbound`, and a mode change
    needs no build. After answering, the controller writes `:pulls` `<pod>` → `<mode> <ms>`
    in a task of its own, so the answer never waits for Valkey (best effort: a failed write is
    counted `mimic_store_writes_total{result="error"}`; at most 16 such writes are in flight,
    and a pull over that is counted `result="dropped"` and writes nothing; neither changes
    the answer). No other answer carries the headers.

  `400` (`format`) for a missing or unknown `format`, `400` for a deployment name that is not
  DNS-1123, `401`/`403`/`429` as §7.2, `405` with `Allow: GET` for another method, `500` with
  ERROR `stored report unreadable` (`key` `outbound`) for an `:outbound` that fails the check
  (never overwritten, §7.3), `503` (`Retry-After: 1`) while Valkey is unavailable, when the
  `TokenReview` fails or when the budgets below are spent, `507` (`egress_limit`) when the
  projection is larger than `MIMIC_SIDECAR_API_MAX_EGRESS` or when the build's body-budget
  share (below) is larger than the whole `MIMIC_SIDECAR_API_BODY_BUDGET` (it could never be
  served: answered without reading `:outbound`, not `503`, so the sidecar does not retry it at
  once), with one WARN `egress limit reached` (`namespace`, `deployment`, `bytes`, `reason`:
  `projection` or `stored`) per deployment and reason per process and
  `mimic_sidecar_api_egress_builds_total{result="too_large"}`.
- **Memory bounds** (controller, as SHR-18). A build (also the one a stale digest forces)
  holds a share of `MIMIC_SIDECAR_API_BODY_BUDGET` equal to the `:outbound` size (`STRLEN`,
  read before the `GET`) plus `MIMIC_SIDECAR_API_MAX_EGRESS` (the serialised projection, which
  is cut at that size and then refused `507`), from before its `GET` until its projection
  holds its egress share (or is refused), so that pushes and builds together, the build's
  output buffer included, stay within the budget SHR-18 measures. Concurrent pulls of the same
  deployment and digest wait for one build (single flight); at most 2 builds run at once per
  replica. A projection holds its size of `MIMIC_SIDECAR_API_EGRESS_BUDGET` while it is cached
  or being sent (all answers of one projection share one share). When a new projection does not fit,
  the least recently served cached projections that are not being sent are dropped; if it
  still does not fit, the pull gets `503`. A cached projection is dropped when a newer digest of
  its deployment is cached.
- **Pull client** (sidecar). With `MIMIC_CONTROLLER_URL` set the sidecar pulls every
  `MIMIC_EGRESS_PULL_INTERVAL` in every mode, the first time after a random delay within one
  interval up to F9 and, from F10, at once at start (so that a pod learns its mode and gets a
  snapshot before its app sends much traffic); without it nothing is pulled (one INFO `egress pull disabled` at start). Each
  pull re-reads the token, sends `If-None-Match` with the `ETag` of the snapshot it holds (none
  before the first), waits at most 30 s for the answer and reads at most
  `MIMIC_EGRESS_MAX_BYTES` of it while it streams. `200` with a body that
  `EgressSet::from_json` accepts replaces the snapshot (an empty set empties it); `304` keeps
  it. *Sidecar memory*: pulls are sequential, so one body at most is read and parsed at a time
  (never two pulls at once, `SIGTERM` flush or not); the body is parsed from its buffer into
  the typed egress set and the buffer is freed before the swap; the snapshot held is dropped
  only after the new one is accepted, so the peak is the old snapshot, the buffer and the new
  set together, and a refused body leaves the old snapshot untouched. That peak, on top of the
  worst case of discovery, must fit the injected memory limit (PUL-18). (F10) The mode of a
  `200` or `304` is applied after the snapshot of the same answer is swapped in (or kept), so
  a sidecar that enters mock answers from the snapshot that came with the mode; a `200` whose
  body is refused (`too_large`, `invalid`) still applies its mode, with the snapshot held.
  An answer without `Mimic-Mode` (a controller up to F9) or with an invalid value keeps the
  current mode (one WARN `mode unknown` per streak, `value` cut to 32 bytes); so does every
  failed pull. Every other outcome keeps the snapshot held, and is counted with its `result`:
  - a body over the cap (`too_large`: the read stops at the cap), a body the check refuses
    (`invalid`, with WARN `egress rejected` and its `reason`), `507` (`too_large`): retried at
    the next interval;
  - `404` or `405` (a controller up to F8, which has no such route): `unsupported`, retried at
    the next interval;
  - a connection error, no answer within 30 s, `401`, `403`, `429`, `5xx` or any other status
    (`error`): retried with the §7.2 backoff (1, 2, 4 … 60 s, or `Retry-After` when longer, at
    most 300 s).

  One WARN `egress pull failed` (`status` or `error`, `result`) per failure streak, INFO
  `egress pull recovered` when a `200` or `304` ends it; DEBUG `egress pulled` (`result`,
  `bytes`, `authorities`, `endpoints`). Logs (`target` = `egress`) never hold the token or
  snapshot content. The snapshot lives in memory only: a restarted sidecar pulls again. At
  `SIGTERM` the pull loop stops at once (a pull in flight is abandoned).
- **Local view.** `GET /egress` on the sidecar admin port, loopback peers only (403 otherwise,
  as `/discovery`), answers a summary of the snapshot, sorted keys: `{"etag": "<ETag>" | null,
  "pulled": "<RFC 3339 of the last 200 or 304>" | null, "authorities": {"<authority>":
  {"endpoints": [{"method", "template", "grpc", "exchanges", "statuses"}]}}}`, from F10 with
  `"mode": "discovery" | "mock"` and `"latency_factor": <number>`. No sample and no
  field name: `/egress` shows what the pod could mock, not the exchanges the other pods
  recorded. The loopback check reads the socket peer, so it assumes the admin port is not
  redirected by the mesh rules and the pod does not use `hostNetwork`.
- **Rollout.** An F9 controller answers pulls whatever the sidecar version (sidecars up to F8
  do not pull); an F9 sidecar facing a controller up to F8 gets `unsupported` and holds an
  empty snapshot. A controller rolled back to F8 writes without the digest; an F9 controller
  that comes back finds it stale and hashes the bytes. The webhook sets no new variable in
  F9. (F10) An F9 sidecar ignores the mode headers; an F10 sidecar facing an F9 controller
  follows `MIMIC_MODE` and keeps it (`mode unknown`), so a controller rollback during a load
  test never turns mock into real egress.
- **Consumers.** F10 adds the mode to what sidecars pull and answers outbound calls from the
  snapshot (§8). Mesh traffic management is v2.

### 7.8 Load test (F11)

An SRE load-tests the ingress of one deployment whose egress is mocked, from its discovered
endpoints, without writing a scenario. Planning (endpoint choice, request building, schedule,
histograms, summaries, gates, the Job object) is pure logic in `mimic-core` (`load` module);
the controller does the Kubernetes and Valkey I/O, the generator `mimic-load` the sockets,
timers and random numbers it hands to the pure functions. Off by default: the chart value
`loadTest.enabled` (default `false`) installs the `MimicLoadTest` CRD, its RBAC and its two
`ValidatingAdmissionPolicy`s and sets `MIMIC_LOAD_MAX_RUNS` (else `0`: the controller watches
no `MimicLoadTest` and creates no Job).

- **`MimicLoadTest`** (namespaced CRD, `mimic.platform.io/v1alpha1`, status subresource),
  in the namespace of the deployment under test:

  ```yaml
  spec:
    deployment: payment        # required, DNS-1123
    duration: 5m               # required, §10 duration, 10s–24h (CRD), ≤ MIMIC_LOAD_MAX_DURATION
    concurrency: 10            # default 10, 1–4096 (CRD), ≤ MIMIC_LOAD_MAX_CONCURRENCY: requests in flight at most
    rate: 200                  # optional, requests/s, 1–100000 (CRD), ≤ MIMIC_LOAD_MAX_RATE; absent: closed model
    rampUp: 30s                # default 0s, ≤ duration
    timeout: 10s               # per request, default 10s, 100ms–60s
    endpoints:                 # optional; entries "<METHOD> <template>" as in the report, at most 64 each
      include: ["POST /payments"]   # absent: every usable inbound endpoint
      exclude: ["DELETE /payments/{payments_id}"]
    credentials:               # optional
      secretName: payment-load # a Secret of this namespace, mounted read-only in the generator
      fields:                  # 1 to 32, distinct `field`s (both keys required when credentials is set)
        - {field: header/authorization, key: token}   # header/<lower-case name> | query/<key> | body<JSON pointer>
    stop: false                # may only go from false to true
  ```

  Every `spec` field is immutable but `stop` (CEL rules). A second
  `ValidatingAdmissionPolicy` refuses a create or update naming `credentials.secretName`
  unless the requesting user may `get` that Secret (CEL `authorizer`), so a `MimicLoadTest`
  never sends a Secret its author could not read. `kubectl get mimicloadtest` prints
  `Deployment`, `Phase`, `Reason`, `Requests`, `RPS`, `P99 µs` and `Age`.
- **Lifecycle** (controller, both replicas, no leader: every step is idempotent). Phases
  `Pending` → `Running` → `Succeeded`, or `Failed` / `Aborted`, with a `reason`:
  1. *Validation* (pure): a value above a `MIMIC_LOAD_*` limit or `rampUp` > `duration` →
     `Failed`/`Invalid` (message names the field); so is a `rate` lower than the number of
     generator pods `n` (below), since every pod must get a rate of at least 1 and the total
     rate stays exact. `MIMIC_LOAD_MAX_RUNS=0` → `Failed`/`Disabled`.
  2. *Gate* (`Pending`, re-evaluated at each `MimicWorkload`, `EndpointSlice` or `:pulls`
     change and every `MIMIC_STATUS_INTERVAL`): the deployment's effective mode (§7) is
     `mock`, it has at least one **target** (below), and every target pod has a `:pulls`
     field within `MIMIC_STATUS_POD_WINDOW` carrying `mock` (§7.4); a field stamped more than
     60 s in the future (clock skew) is stale, not fresh. Until then `reason`
     `WaitingForMock` or `WaitingForTargets`; after `MIMIC_LOAD_START_TIMEOUT` →
     `Failed`/`NotMock` or `NoTargets`. No Job exists while `Pending`. At most
     `MIMIC_LOAD_MAX_PENDING` runs of a namespace are `Pending`: the oldest by
     `creationTimestamp` (then name) are kept, a newer one is `Failed`/`TooManyPending` (both
     replicas see the same objects, so they decide the same).
  3. *Admission* (one Valkey script): refused when the deployment already has an active run
     (`Failed`/`Busy`), its namespace has `MIMIC_LOAD_MAX_NAMESPACE_RUNS` (`Failed`/
     `NamespaceBusy`), the cluster has `MIMIC_LOAD_MAX_RUNS` (`Failed`/`ClusterBusy`), or the
     run's reservation (its plan's bytes + pods × 256 KiB + 192 KiB) would bring the
     reservations of the active runs over `MIMIC_LOAD_STORE_BUDGET` (`Failed`/`StoreFull`);
     otherwise the run takes a slot and its reservation, and its **plan** (below), built
     from the stored report on the blocking render pool of §7.5, is written with it. No
     report → `Failed`/`NoReport`; a plan without endpoint → `Failed`/`NoEndpoints`
     (`status.skipped` gives the counts per reason). The **load ticket** is signed then
     (below) and stored with the plan.
  4. *Job*: the controller creates the Job below (name `mimic-load-<first 12 hex of the
     run's uid>`, `AlreadyExists` is success) → `Running`, `status.startTime`.
  5. *End*: every generator pod posted its final results → `Succeeded`/`Completed`; the Job
     failed (pod error, `activeDeadlineSeconds`) → `Failed`/`JobFailed`; a generator reported
     a refusal → `Aborted`/`ModeLeft`, an unverified answer → `Aborted`/`Unverified`, no verified target → `Failed`/`NoTargets`.
     `spec.stop: true` → `Aborted`/`Stopped`; the effective mode leaving `mock` →
     `Aborted`/`ModeLeft`. On `Stopped` and `ModeLeft` the controller deletes the Job
     (propagation `Background`) at the watch event, and the run's Valkey `stop` flag is set so
     that the next generator poll stops it. At every end, in one Valkey script, the run's
     `plan`, `ticket` and per-pod fields are deleted and replaced by `summary` (the `results`
     of the status below, at most 192 KiB: 64 endpoints of templates up to 2 KiB), the slot
     and the reservation are released, and `status.endTime` set; a post arriving afterwards
     is `404`. The end script refuses a summary over 192 KiB; the controller keeps its summary at
     most 184 KiB by dropping the per-endpoint rows (the total stays) when the full one is larger.
     Deleting the `MimicLoadTest` deletes the Job (owner reference, `blockOwnerDeletion:
     false`) and releases the slot. A delete event a replica misses leaks nothing: the slot's
     `loads:active` score is the run's deadline (start + `activeDeadlineSeconds`), and the next
     admission drops the member and its reservation once it is past (LDT-09). A replica that
     loses the end script, or finds the run ended, copies `phase`, `reason`, `summary` and
     `pods` from the stored run, never from its own (possibly stale) view of the status.
     A re-list of the `MimicLoadTest`s after a watch gap compares uids: an object that is
     gone, or has been recreated under the same name with another uid, ends the old uid's run
     as if it had been deleted (Job deleted, `Aborted`/`Stopped`, slot released). A run ending
     in `Pending` whose slot cannot be read (Valkey down) logs one WARN `load test end
     deferred` per outage and is retried at the next pass.
  6. *Pacing and outages*. A pass runs every 500 ms. A replica reads a running run's pods'
     results (`load_posts`) every 500 ms while they change; when a read finds them unchanged
     the interval doubles, up to 4 s, and returns to 500 ms at the next change (a final post
     is therefore seen within 4 s). While a run in `Pending` waits for the signing seed, the
     webhook CA or Valkey, its plan is built once and kept for the next passes (never rebuilt
     at every pass), and the wait is logged once per outage as one WARN `load test admission
     waiting` (`reason`: `seed`, `ca` or `valkey`), the next one only after the run has
     progressed.
- **Targets**: the ready endpoints (`conditions.ready` true or absent) of every
  `EndpointSlice` of the namespace whose `targetRef` is a `Pod` with a name the deployment's
  ReplicaSets can generate (the §7.2 check), one address per pod (the first), deduplicated.
  A pod that backs no Service is not a target. From F11 the injection patch (§7.1) also
  labels every injected pod `mimic.platform.io/deployment: <deployment>` (INJ-03; a deployment whose name is longer than 63 characters, the limit of a label value, gets no label and no target, and a label under that key which the pod already carries is removed), which the generator
  NetworkPolicy selects. The generator never resolves a name nor uses
  a Service address: it connects only to these pod addresses, on the ports of the plan.
- **Plan** (pure, `mimic-core` `load::plan`, from the stored inbound report as re-redacted by
  §7.3 and the spec). Only inbound endpoints. An endpoint is *skipped*, counted by reason in
  `status.skipped`, when it is `grpc` (recorded messages are opaque, §6: `grpc`), was
  answered `101` (`upgrade`), is not in `include` or is in `exclude` (`excluded`), has no
  usable sample (`no_sample`), or every usable sample needs a credential the spec does not
  supply (`credentials`) or has a request body that was not kept (`opaque_body`: its request
  has a `content-type` header but the report holds no body, because the recorder keeps none
  for an oversize, unparsable or other than JSON or form body; sending it without body would
  not be the recorded request, so such a sample is unusable and an endpoint with only such
  samples is `opaque_body`). Usable
  samples: status 200–399, else, when the endpoint has none, 400–599. A sample *needs a
  credential* when a header, query value or body leaf of its request holds `<redacted>` (or is
  a field marked `redacted`); it is usable only when every such field is in
  `credentials.fields`. A name cannot be supplied: a sample whose path segment, header name,
  query key, form key or JSON object key holds `<redacted>` is never usable and counts as
  `credentials`, whatever the spec supplies. The plan keeps at most
  `MIMIC_LOAD_MAX_ENDPOINTS` endpoints, those with the most exchanges (ties: endpoint key
  order; the others `limit`), each with its usable samples, its `volatile` list, its port, its
  first host in byte order (else none) and a weight equal to its exchange count. While the
  serialised plan exceeds `MIMIC_LOAD_MAX_PLAN` the sample with the highest content hash is
  dropped (an endpoint left without sample is `plan_size`). The same report and spec give the
  same bytes (`BTreeMap`). The plan holds no credential: only field names and Secret keys.
- **Requests** (pure, `load::request`, from a plan endpoint, the caller's random input and
  time): an endpoint drawn by weight, then one of its samples uniformly. Method, concrete
  path and query of the sample; its headers without `host`, `content-length`,
  `content-encoding`, `transfer-encoding`, `connection`, `keep-alive`, `te`, `trailer`,
  `upgrade`, `expect` and `proxy-*`; `host` set to the endpoint's host, else `<ip>:<port>`;
  the body serialised compactly (JSON) or form-encoded, with `content-length`; plus
  `mimic-load-run: <ticket>`. Every field of the endpoint's `volatile`
  list gets a fresh value: a `date-time` or `date` value the current time in its format, a
  number a random one with as many digits, any other string the same characters with each
  digit replaced by a random digit and each letter by a random letter of its case (only
  `a-f` when every letter of the value is a hex digit), other characters kept (so a UUID, a
  `traceparent`, an idempotency key keep their shape). Each `credentials.fields` entry
  replaces its field's whole value with the bytes of the Secret key (trailing newline
  removed). No request holds `<redacted>`. HTTP/1.1 only, cleartext (inbound TLS is not
  discovered, §5), keep-alive.
- **Schedule** (pure, `load::schedule`). The Job runs `n = ceil(concurrency /
  MIMIC_LOAD_POD_CONCURRENCY)` pods (Indexed, `parallelism` = `completions` = `n`); pod `i`
  gets `concurrency / n` and `rate / n`, the remainders going to the lowest indices.
  *Open model* (`rate` set): sends are scheduled at a rate rising linearly from 0 to the
  pod's rate during `rampUp`, then constant until `duration` (from the pod's own start); a
  send due while the pod's concurrency is in flight is not sent and counted `missed` (no
  queue, so saturation shows as `missed` instead of hidden latency). *Closed model* (no
  `rate`): workers loop back to back, their number rising linearly from 1 to the pod's
  concurrency during `rampUp`. No request starts after `duration`; requests in flight then
  get `timeout` to finish.
- **Target check**, the hard guarantee that load reaches only the deployment under test in
  mock, at three levels:
  - *Probe* (generator): before each new connection to a target the generator gets
    `http://<pod ip>:15100/load` (the sidecar admin port, never redirected, §4) and connects
    only if it answers `200` JSON `{"deployment", "mode", "namespace", "pod"}` with the run's
    namespace and deployment, the target's pod name and `mode` `mock`. Any other answer, or none
    within 2 s: no connection, the target is skipped until the next targets poll (WARN
    `target not verified`); no verified target for `MIMIC_LOAD_START_TIMEOUT` → final post
    with `NoTargets`.
  - *Load ticket*: `v1.<payload>.<signature>` (base64url without padding), the payload the
    canonical JSON `{"deployment", "exp", "namespace", "uid"}` (`exp`: the run's deadline,
    Unix seconds), the signature ECDSA P-256 SHA-256 (fixed-length `r‖s`) by the deployment's
    root key (§5) over the bytes `mimic-load-ticket\0` followed by the payload (the prefix
    cannot start a DER certificate, so a ticket never signs anything else). The controller
    signs it at admission; the sidecar verifies it with the public key of `MIMIC_TLS_CA`, and
    keeps at most 16 verified tickets (the last seen), so a request costs a lookup.
  - *Sidecar* (per request, on the target pod, §4): an inbound HTTP/1 request or `h2` stream
    carrying `mimic-load-run` (on a pod with `MIMIC_LOAD_ENABLED=1`; otherwise the header is an
    ordinary one and `GET /load` answers `404`) is forwarded only when the sidecar's mode, read at its first
    byte, is `mock` and the header holds a ticket that verifies, is not expired, and names the
    sidecar's own namespace and deployment; it is then forwarded without that header, every
    `x-mimic-load` header of the app's answer is removed and `x-mimic-load: ok` added, and it
    is not recorded (`mimic_discovery_exchanges_total{result="load"}`). Otherwise it is
    answered by the sidecar `599`, `x-mimic-load: refused; reason=<reason>`, `content-length:
    0`, the app receiving nothing (not one byte: an `h2` sidecar opens its connection to the app
    only for the first stream it forwards); `reason` is `mode`, `workload` (another namespace or
    deployment) or `ticket` (malformed, bad signature, expired, or no `MIMIC_TLS_CA`), one WARN
    `load request refused` per reason per mode period. An F10 sidecar has no `/load` route,
    so no load reaches it.
  - *Generator*: an answer with `x-mimic-load: refused; reason=mode` stops every request of
    the pod at once (`ModeLeft`); any other refusal and an answer without `x-mimic-load: ok`
    do the same with `Unverified`; the pod posts its final results with that reason; the
    controller then ends the run (`Aborted`) and the next poll of every other pod answers
    `stop`.
  - *Controller*: the effective mode leaving `mock` ends the run (`Aborted`/`ModeLeft`) as in
    step 5.
- **Generator** `mimic-load` (in the `mimic` image), one Job per run in the deployment's
  namespace. The container declares its metrics port 15100, `protocol: TCP`, named
  `mimic-admin` (as the sidecar, §7.1, MON-03; no `hostPort`). Pod: `automountServiceAccountToken: false`, the namespace's `default` service
  account, a projected token of audience `mimic.platform.io/load` (600 s) as the only
  credential towards mimic, UID 65532, `runAsNonRoot`, read-only root, all capabilities
  dropped, `RuntimeDefault`, `MIMIC_LOAD_RESOURCES`, labels `mimic.platform.io/inject:
  disabled`, `app.kubernetes.io/managed-by: mimic`, `mimic.platform.io/load-test: <name>`, the
  credentials Secret (when given) mounted at `/mimic/credentials` (`optional: false`, mode
  0400), and then the Job annotation `mimic.platform.io/credentials-secret: <secretName>`
  (§11). Job: `backoffLimit: 0` (a retried pod would send unplanned load),
  `activeDeadlineSeconds` = `duration` + `MIMIC_LOAD_START_TIMEOUT` + 60 s,
  `ttlSecondsAfterFinished: 3600`, owner reference to the `MimicLoadTest`. Container
  `mimic-load`, the only one (no init or ephemeral container), command
  `/usr/local/bin/mimic-load`, no `args`, `env` literal values of the §10.6 names only (no
  `valueFrom`, no `envFrom`): the pod learns its index from the load API, which reads it
  from the pod name of the token. No `nodeName`, toleration or `hostAliases`.
  *NetworkPolicy* (`loadTest.networkPolicy`, default `true`; created with the Job, same owner
  and labels, deleted with it): pods `mimic.platform.io/load-test: <name>`, no ingress but the
  admin port from `loadTest.metricsFrom` (default none); egress only to pods labelled
  `mimic.platform.io/deployment: <deployment>` (set by the webhook on every injected pod,
  LDT-20) on the plan's ports and 15100, to the controller pods on the sidecar API port, and
  to `loadTest.dnsSelector` (default namespace `kube-system`, pods `k8s-app: kube-dns`) on
  port 53 UDP and TCP for the controller's Service name. It is a second fence behind the
  probe; a CNI without NetworkPolicy support ignores it (§11.1). The pod reads the
  plan once, then polls the targets every `MIMIC_LOAD_REPORT_INTERVAL` and posts its cumulative
  results at the same interval, at the end and at `SIGTERM` (within `MIMIC_SHUTDOWN_GRACE`). It
  exits 0 after a completed run and 1 after an aborted one. Without an answer of the load API
  for 3 intervals it stops its requests (no load without control), posts when it can and exits 1.
- **Load API** (controller, on the sidecar API listener of §7.2, with its admission order,
  rate limit and `TokenReview` limits). Routes `/v1/load/<name>/<uid>/…`: the token must have
  audience `mimic.platform.io/load` (a sidecar token there, and a load token on the routes of
  §7.2, are `401`), its namespace the run's, and its bound pod a name the run's Job can
  generate (`<job>-<index>-<5 chars of the §7.2 alphabet>`, else `403`); `404` for an unknown
  or ended run.
  - `GET …/plan` → `{"plan": <the plan stored at admission>, "ticket", "worker": <index
    from the pod name>, "workers": <pods of the Job>, "params": {"concurrency", "rate",
    "durationMs", "rampUpMs", "timeoutMs", "startTimeoutMs"}}` (`application/json`); `params`
    is the pod's own share (`split`: its `concurrency` and its `rate`, `null` in the closed
    model) and the run's durations, so the Job needs no variable for them.
  - `GET …/targets` → `{"stop": <bool>, "targets": [{"ip", "pod"}]}`, sorted by pod; `503` while the
    controller's `EndpointSlice` cache has not synced (an empty list would read as `NoTargets`).
  - `POST …/results` (at most 256 KiB, else `413`) `{"final": <bool>, "pod", "reason":
    null | "Completed" | "ModeLeft" | "Unverified" | "NoTargets", "results": <cumulative
    results of this pod>}` → `200` `{"stop": <bool>}`. A post replaces the previous results
    of the same worker index (cumulative, so a retry never double-counts; the results are kept per
    index, so a generator pod that the Job replaces takes over its predecessor's field and the
    stored bytes stay within the bound of LDT-09).
- **Results** (pure, `load::summary`). Per endpoint (method, port, template) and in total:
  `requests`, `missed`, `statuses` by class (`2xx` … `5xx`), `errors` by kind (`timeout`,
  `connect`, `reset`, `protocol`, `refused`, `unverified`), `rps` (requests × 1000 / elapsed
  ms of the run, rounded) and `latencyMicros` `{p50, p90, p99, max}`: time from the first
  byte written to the response head, in a histogram of 16 buckets per doubling from 1 µs to
  300 s plus overflow (a percentile is the upper bound of its bucket: at most 4.43 % above the
  exact value, 2^(1/16) − 1), merged over pods by addition (commutative, associative). They hold method,
  port, template and numbers only: no header, query, body, address or error text.
  Stored under `:load:<uid>` (§7.3); every `MIMIC_STATUS_INTERVAL` and at each phase change the
  controller applies `status` (server-side apply, field manager `mimic-controller`):

  ```yaml
  status:
    phase: Running            # Pending | Running | Succeeded | Failed | Aborted
    reason: ""                # see Lifecycle
    message: ""               # human text: names of fields, endpoints and limits only
    startTime: "…"            # Job created; endTime when ended
    targets: 2                # at the last targets answer
    pods: 1                   # generator pods that posted
    skipped: {credentials: 1, grpc: 2}
    results:
      total: {requests, missed, statuses, errors, rps, latencyMicros}
      endpoints: [{method, port, template, requests, missed, statuses, errors, rps, latencyMicros}]  # plan order, ≤ MIMIC_LOAD_MAX_ENDPOINTS
  ```
- **Consumers.** From the F11 UI step the read API lists a deployment's runs (§7.5:
  `GET /v1/workloads/<ns>/<dep>/loadtests` and `…/loadtests/<uid>`, the stored summaries) and the deployment page shows them. Scripted multi-step scenarios, gRPC and
  inbound TLS load, and load across clusters are v2. Mesh traffic management is v2.

## 8. Mock

F10. The sidecar answers the app's outbound HTTP from the egress snapshot it pulled (§7.7),
never from another deployment's data: the snapshot is the only source, and the controller
serves a pod its own deployment only (§7.2). Matching, response building and the latency draw
are pure logic in `mimic-core` (`mock` module); the sidecar does the I/O, the timers and the
random numbers it hands to the draw.

- **Mode state.** At start the mode and latency factor come from the state file
  `/mimic/state/mode` when it holds a valid `{"latency_factor": <n>, "mode": "<mode>"}`, else
  from `MIMIC_MODE` and `MIMIC_MOCK_LATENCY_FACTOR`; from then on, from each pull answer
  (§7.7). The sidecar writes the state file (write to a temporary file, then rename) at each
  change, before it applies the change to traffic. `/mimic` is the `mimic-trust` `emptyDir`
  (§7.1): it survives a restart of the sidecar container, not the pod (a new pod gets the mode
  of its admission), and app containers only mount its `trust/<n>` subpaths read-only, so the
  app can neither read nor write the file. An unreadable or invalid file is ignored with one
  WARN `mode state ignored`; a failed write is one WARN `mode state not saved` and changes
  nothing in traffic. One INFO `mode changed` (`target` = `mock`, `mode`, `previous`,
  `latency_factor`, `source`: `env` | `state` | `pull`) at start and at each change of mode or
  factor. The latency factor is a number of hundredths, 0–10000 (the controller and the
  sidecar round a written factor to the nearest hundredth, halves up). The snapshot is pulled and refreshed in both
  modes; an empty snapshot in mock answers every request `599`.
- **Connections in mock.** The sidecar does not connect to the destination before it knows
  what the connection is. It waits for the client's first bytes (detection of §4.1):
  - an HTTP/1.x request line or the HTTP/2 preface: a **mocked connection**, served by the
    sidecar itself, with no connection to the destination, unless the target authority of its
    first request matches `MIMIC_MOCK_PASSTHROUGH` (below);
  - a TLS ClientHello: §5 "Mock mode"; an intercepted stream whose decrypted bytes are HTTP is
    a mocked connection, any other decrypted stream is closed (`result: mock_closed`), and so
    is one that sends no byte for 30 s (nothing is connected, so no server could speak first;
    the stall of an incomplete prefix applies from the handshake on);
  - other client bytes (a client-first non-HTTP protocol: PostgreSQL, Redis, Kafka …):
    connected and relayed as an opaque tunnel, as in discovery, and recorded as a `tcp` tunnel
    (§6.1): mimic cannot answer them, and the workload under test needs its own datastores;
  - no byte within **1 s**: the sidecar connects (a server-first protocol: MySQL, SMTP) and
    keeps waiting for the first bytes of either side; the destination first, or non-HTTP
    client bytes: an opaque tunnel; an HTTP request line, the preface or a ClientHello from the
    client before the destination spoke: the destination socket is closed without a byte
    written to it and the connection is handled as above. Only a TCP handshake reaches such a
    destination.
  - Each connection is counted once in `mimic_mock_connections_total{decision}`: `mocked`,
    `tunnel` (an opaque tunnel), `passthrough` or `closed`.
  - **Passthrough** (`MIMIC_MOCK_PASSTHROUGH`, from `spec.mock.passthrough`, grammar and
    matching of `spec.tls.exclude`, §5): a connection whose first request's target authority
    host, SNI, or destination IP (IP literal entries) matches is handled exactly as in
    discovery (connected, forwarded, recorded), for its whole life. With a passthrough list,
    an HTTP/1 connection is held until its first request head is complete (within the stall
    of an incomplete prefix) to read the authority; the authority of an `h2c` connection is
    not read: only the destination IP entries match it. It is how internal
    services the load test needs (an auth service, a TLS datastore excluded from interception)
    stay real. Applied at pod creation, like `spec.tls.exclude`.
- **Requests on a mocked connection.** HTTP/1.1 (keep-alive, pipelining, `Expect:
  100-continue` answered `100`) and HTTP/2 (each stream on its own, concurrently) are served by
  the sidecar's own `hyper` server; mocked answers are mimic's, so the byte exactness of §4.1
  does not apply to them (the server may add `date`). The request body is read to its end: its
  first 256 KiB are kept for matching within `MIMIC_DISCOVERY_CAPTURE_BUDGET` (decoded and
  parsed as §6; a body over the limit, opaque, undecodable or that does not fit the budget is
  matched as absent), the rest is read and discarded. A request the server cannot parse closes
  the connection (`result: unframed` or `rejected`, as §4.1); the head limits of the server
  are 64 KiB and 256 lines, as §4.1's. A parse error on a pipelined request, while an earlier
  answer is still unflushed, can let `hyper`'s own `400` out: it carries no data and reaches no
  destination, and the `unframed` line and count are written either way. The 30 s header-read
  timeout also closes idle keep-alive connections on the mocked path, and an HTTP/2 connection
  with no stream open for 30 s (after the preface too, counted from the end of the last stream) gets `GOAWAY` and is closed (the
  no-idle-timeout rule of §4.1 holds for forwarded connections only); `MIMIC_MAX_CONNECTIONS`
  is the real bound. A
  `CONNECT` and a request that asks for an upgrade (`Upgrade`, extended `CONNECT`) are misses
  (`connect`, `upgrade`).
- **Request signature** (pure). Authority: the scope key of §6.1 (target authority, else SNI,
  else destination `ip:port`). Path: the concrete path redacted as samples are (§6 value
  detectors). Query pairs and body: redacted with the outbound request view (§6.1), as the
  recorder would have redacted them. Header values are never compared.
- **Endpoint lookup.** Among the endpoints of the request's authority (none: miss
  `authority`) with the request's method (exact, `HEAD` is not `GET`): a gRPC request
  (`content-type: application/grpc*` on HTTP/2) takes the `grpc` endpoint whose template equals
  its redacted path; any other request the templates with as many segments as its path where
  every literal equals the segment (raw bytes) and every `{param}` takes any segment, the most
  specific first (at the first position where two templates differ, a literal beats a
  parameter). None: miss `endpoint`.
- **Fields** of a request or of a sample's request: `path/<n>` (the segment at each parameter
  position of the template), `query/<key>` (its values in order), and the leaves of a parsed
  body `request.body/<media><pointer>` (JSON pointer, form field `/<key>`). A field whose §6
  name (array indices written `*`) is in the endpoint's `volatile` or `redacted` list, or whose
  value holds the marker `<redacted>`, is left out on both sides: it is never compared.
- **Tiers**, first that gives a sample wins, samples taken in snapshot order, `1xx` samples
  never used:
  1. `exact`: the first sample whose fields are exactly the request's (same names, same
     values; same body media type, or both without body);
  2. `endpoint`: the sample that shares the most fields (same name and value) with the
     request, at least one; ties go to the first;
  3. `fallback`: the endpoint's most frequent status other than `1xx` (ties: the lowest); the
     first sample with that status, else that status alone with an empty body. An endpoint
     with only `1xx` statuses is a miss (`endpoint`).
- **Answer.** The sample's status and response headers in their order, without
  `content-length`, `content-encoding`, `transfer-encoding`, `connection`, `keep-alive`,
  `te`, `trailer`, `upgrade` and `proxy-connection`, plus `content-length` of the body sent and
  `x-mimic-match: exact|endpoint|fallback`. Body: the sample's JSON value serialised compactly,
  or its form pairs encoded as `application/x-www-form-urlencoded`, identity coding; a sample
  without body (opaque, too large) gives an empty body; `HEAD`, `204` and `304` have none.
  Redacted values are sent as recorded (`<redacted>`): a mock never holds, and so never
  returns, a secret (§11.1). **gRPC**: status 200, the sample's headers (`content-type`
  kept), one message of length zero (5 bytes `00 00 00 00 00`, the default value of any
  message), then trailers `grpc-status: 0`; recorded messages are opaque and not kept (§6), so
  a gRPC mock always succeeds with an empty message.
- **Latency.** Drawn from the endpoint's recorded histogram (§6), never from the sample: a
  bucket with probability count / total, then a whole number of milliseconds uniformly within
  it (bucket `i` holds the integers in (2^((i-1)/4), 2^(i/4)], bucket 0 holds 0 and 1, the
  overflow bucket gives 131 072); multiplied by the latency factor in hundredths `f` as `(d × f + 50) / 100` in integer
  milliseconds (rounded to the nearest, halves up; `0` disables it) and capped at 300 s; an
  empty histogram gives 0. The response head is sent no earlier than that delay after the first
  byte of the request was read (the definition of `head_ms`, §4.1), the body right after it.
  A miss is answered without delay.
- **Miss.** Status **599**, headers `x-mimic-miss: 1` and `content-length: 0`, empty body,
  gRPC included (its client sees an HTTP error, not a `grpc-status`). The real destination is
  never called. The overload answer of "Bounds" has the same form. One WARN `mock miss`
  (`reason`: `authority`, `endpoint`, `connect`, `upgrade`; `overload` is logged as "Bounds"
  says, not here; `authority`, `method`, `path` as in the access line: redacted, no query) per
  distinct (`reason`, `authority`, `method`, `path`) per mode period (from one entry into
  mock to the next), at most 256 lines per period, then one WARN `mock miss log limit reached` until the mode changes; every miss is
  counted.
- **Not recorded.** A mocked exchange is not discovery: it creates no exchange record (not in
  `mimic_discovery_exchanges_total`), so a mock never feeds its own snapshot. Inbound
  exchanges, tunnels and passthrough connections are recorded as in discovery.
- **Access lines and metrics.** A mocked exchange writes its `http exchange` line with `mock`:
  `exact` | `endpoint` | `fallback` | `miss` and, for a miss, `mock_miss` (the reason); it is
  counted in the RED metrics (`direction="outbound"`) like a forwarded one. A connection closed
  by mock writes a `connection` line with `result: mock_closed` (and its `interception` for
  TLS). `mimic_mock_*` metrics: §9.
- **Switching.** The mode is read when a request's first byte is read and when a connection's
  kind is decided:
  - *discovery → mock*: on a forwarded HTTP connection (HTTP/1 or h2, cleartext or
    intercepted) a request read after the switch is answered by the mock, never forwarded;
    requests forwarded before it get the destination's answers, then the destination
    connection is closed (FIN; `GOAWAY` for h2) and the connection continues as mocked. Open
    tunnels that mock would not keep (TLS passed through, decrypted opaque streams, `CONNECT`
    and upgraded tunnels) are closed at once (`result: mock_closed`); non-HTTP cleartext
    tunnels and passthrough connections continue.
  - *mock → discovery*: on a mocked connection the requests read before the switch are
    answered (latency included); then an HTTP/1 connection is closed (FIN) and an HTTP/2 one
    gets `GOAWAY` naming its last stream opened before the switch. New connections are handled
    as in discovery (connected, forwarded, recorded). The snapshot is kept.
- **Bounds.** Matching cost is bounded by the snapshot bounds (§7.7: 20 samples per endpoint,
  128 field names) and the 256 KiB body. The *index* built from an accepted snapshot replaces
  the typed set (it is not held twice) and holds each sample's answer once, serialised (status,
  headers, body), behind an `Arc`; PUL-18 measures it in place of the typed set.
  - *Pending answers share, never copy.* A mocked request is *pending* from the first byte of
    its request to the end of its answer. Its matched request body is held in
    `MIMIC_DISCOVERY_CAPTURE_BUDGET` only until matching ends, before the latency wait; then it
    holds an `Arc` of the chosen answer (and of the index it belongs to, so a snapshot swapped
    meanwhile is freed when its last pending answer ends) and a timer, nothing else.
  - *Cap.* At most `MIMIC_MOCK_MAX_PENDING` requests are pending at once (gauge
    `mimic_mock_pending`). One more is answered at once, without reading its body, with an
    *overload* answer: `599`, `x-mimic-miss: 1`, `content-length: 0`, counted as a miss with
    reason `overload` (access `mock_miss: overload`); an HTTP/1 connection is then closed (its
    unread body cannot be skipped safely), an HTTP/2 stream is ended with `RST_STREAM`
    (`NO_ERROR`) after the answer. One WARN `mock overload` (`limit`) when the cap is reached,
    again only after the pending count fell below 7/8 of the cap (INFO `mock overload ended`).
    Mocked `h2` connections advertise `SETTINGS_MAX_CONCURRENT_STREAMS` 256.
  - *Cancellation.* When the client aborts (closes or resets the connection, or resets the
    stream) the pending request is dropped at once: its timer is cancelled, nothing is written,
    the access line says `client_abort`, and its pending slot and `Arc` are released. At
    `SIGTERM` pending answers are dropped the same way when the grace of §4.1 ends.
  - The default cap keeps the worst case (every pending answer sharing one 256 KiB sample, or
    all different) within the 128 Mi sidecar limit with the PUL-18 snapshot held (MCK-18):
    answers are shared, so pending requests cost their task and stream state only.
- Mesh traffic management (retries, fault injection, routing) is v2 (§1.2).

## 9. Observability

- **Logs**: JSON lines on stdout, one object per event, with at least `timestamp`, `level`,
  `target` and `message`. Access logs (one line per exchange, `target` = `access`) and
  component logs. Level from `MIMIC_LOG` (`tracing` filter syntax, default `info`).
- **Metrics**: OpenMetrics text format (Prometheus-compatible) on the admin port at `/metrics`. Every component exposes
  `mimic_build_info{component,version} 1` and `mimic_log_lines_dropped_total` (§4.1), and from
  #385 the `process_*` metrics below. The sidecar exposes RED metrics with bounded cardinality:

  | Metric | Type | Labels |
  |---|---|---|
  | `mimic_http_requests_total` | counter | `direction`, `protocol` (`http1`, `h2`), `authority`, `status_class` (`1xx`…`5xx`, `none` when no response) |
  | `mimic_http_request_duration_seconds` | histogram, time to the response head; buckets 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10, 30, 60 | `direction`, `authority`, `status_class` |
  | `mimic_proxy_connections_total` | counter | `direction`, `protocol` (`http1`, `h2`, `tls`, `tcp`), `authority` (SNI for TLS when present, else the original destination `ip:port`) |
  | `mimic_proxy_bytes_total` | counter | `direction`, `protocol`, `flow` (`to_upstream`, `to_client`) |
  | `mimic_tcp_connections_total` | counter (F1) | `direction`, `result` (`ok`, `connect_error`, `loop`, `no_original_dst`) |
  | `mimic_accept_errors_total` | counter | `direction` |
  | `mimic_tls_connections_total` | counter, outbound TLS connections by §5 decision | `interception` (`intercepted`, `excluded`, `no_ca`, `unsupported`, `upstream_untrusted`, `client_cert_requested`, `client_rejected`) |
  | `mimic_tls_passthrough_destinations` | gauge, remembered destinations (§5) | — |
  | `mimic_tls_intermediate_expiry_timestamp_seconds` | gauge, `notAfter` of the current intermediate, 0 when none | — |
  | `mimic_tls_intermediate_requests_total` | counter | `result` (`ok`, `error`, `mismatch`) |
  | `mimic_discovery_exchanges_total` | counter (F4), HTTP exchanges seen by discovery | `direction`, `result` (`recorded`, `not_found`, `invalid`, `endpoint_limit`, `authority_limit` (F7, outbound), `dropped`, `panic`, from F11 `load`: an inbound load request forwarded and not recorded, §7.8) |
  | `mimic_discovery_bodies_total` | counter (F4), bodies of recorded exchanges | `direction`, `part` (`request`, `response`), `result` (`parsed`, `empty`, `opaque`, `too_large`, `encoding`, `invalid`, `budget`) |
  | `mimic_discovery_samples_total` | counter (F4), sample admissions | `direction`, `result` (`kept`, `replaced`, `rejected`, `budget`) |
  | `mimic_discovery_redactions_total` | counter (F4) | `location` (`path`, `query`, `header`, `body`), `rule` (`name`, `jwt`, `pem`, `credential`, `card`, `aws`, `token_prefix`, `userinfo`, from F7 `outbound_header`, §6.1) |
  | `mimic_discovery_endpoints` | gauge (F4), endpoints in the template tree; after a freeze (§7.2) those of the pending and the current report together, each distinct endpoint once | `direction` |
  | `mimic_discovery_sample_bytes` | gauge (F4), bytes held by samples; after a freeze those of the pending and the current report together; from F7 the outbound part included | — |
  | `mimic_discovery_authorities` | gauge (F7), outbound scopes held (§6.1) | — |
  | `mimic_discovery_tunnels_total` | counter (F7), outbound connections without plaintext HTTP (§6.1) | `kind` (`tcp`, `connect`, `tls_excluded`, `tls_no_ca`, `tls_unsupported`, `tls_upstream_untrusted`, `tls_client_cert_requested`, `tls_client_rejected`), `result` (`recorded`, `authority_limit`, `dropped`) |
  | `mimic_discovery_pushes_total` | counter (F5), settled or failed push attempts (§7.2) | `result` (`ok`, `duplicate`, `too_large`, `rejected`, `error`, from F8 `inbound_only`: a version 2 report answered `400` or `507`, followed by the fallback of §7.2) |
  | `mimic_discovery_outbound_dropped_total` | counter (F8), outbound parts dropped by the inbound-only fallback after an attempt with an unknown outcome (§7.2) | — |
  | `mimic_discovery_last_push_timestamp_seconds` | gauge (F5), time of the last acknowledged push, 0 before | — |
  | `mimic_egress_pulls_total` | counter (F9, sidecar), egress pull attempts (§7.7) | `result` (`ok`, `not_modified`, `too_large`, `invalid`, `unsupported`, `error`) |
  | `mimic_egress_endpoints` | gauge (F9, sidecar), endpoints in the egress snapshot | — |
  | `mimic_egress_bytes` | gauge (F9, sidecar), size of the snapshot's body as received, 0 without one | — |
  | `mimic_egress_last_pull_timestamp_seconds` | gauge (F9, sidecar), time of the last `200` or `304` pull, 0 before | — |
  | `mimic_mock_mode` | gauge (F10, sidecar), 1 in mock, 0 in discovery (§8) | — |
  | `mimic_mock_requests_total` | counter (F10), outbound requests answered by the mock (§8) | `result` (`exact`, `endpoint`, `fallback`, `miss`) |
  | `mimic_mock_miss_total` | counter (F10), misses answered `599` | `reason` (`authority`, `endpoint`, `connect`, `upgrade`, `overload`) |
  | `mimic_mock_pending` | gauge (F10), mocked requests pending (§8 "Bounds") | — |
  | `mimic_mock_connections_total` | counter (F10), outbound connections whose kind was decided in mock | `decision` (`mocked`, `tunnel`: non-HTTP relayed, `passthrough`, `closed`) |
  | `mimic_load_inbound_total` | counter (F11, sidecar), inbound requests carrying `mimic-load-run` (§7.8) | `result` (`forwarded`, `refused_mode`, `refused_workload`, `refused_ticket`) |

  An intercepted connection is counted in `mimic_proxy_connections_total` and
  `mimic_proxy_bytes_total` under its decrypted protocol (`http1`, `h2`, or `tcp` for an
  opaque decrypted tunnel), with the SNI as `authority` when present; a passed-through one
  under `tls` as before. The controller exposes `mimic_sidecar_api_requests_total` (§7.2) and
  `mimic_tls_seed_loaded` (gauge, 1 once the seed is loaded), and from F5 (no label carries a
  namespace, deployment or pod, so cardinality stays fixed):

  | Metric | Type | Labels |
  |---|---|---|
  | `mimic_store_writes_total` | counter, store write attempts (§7.3); `refused` is an attempt the script refused for `MIMIC_STORE_MAX_DEPLOYMENTS` (`507`), which writes nothing | `result` (`ok`, `conflict`, `error`, `refused`, from F10 `dropped`: a `:pulls` write (§7.7) not started because 16 were already in flight) |
  | `mimic_store_write_duration_seconds` | histogram, one write from read to script result; buckets 0.005 … 10 as `mimic_http_request_duration_seconds` | — |
  | `mimic_store_report_bytes` | histogram, size of each report written; buckets 4Ki, 16Ki, 64Ki, 256Ki, 1Mi, 4Mi, 16Mi, 64Mi | — |
  | `mimic_store_pushes_total` | counter, pushes by outcome | `result` (`applied`, `duplicate`) |
  | `mimic_store_refused_total` | counter, growth refused by the bounded merge | `kind` (`endpoint`, `sample`, from F8 `authority`), `reason` (`endpoint_limit`, `budget`, from F8 `authority_limit`) |
  | `mimic_store_evicted_total` | counter, stored endpoints and samples displaced by the bounded merge (§7.3) | `kind` (`endpoint`, `sample`, from F8 `authority`) |
  | `mimic_store_expired_total` | counter, endpoints and samples dropped by retention | `kind` |
  | `mimic_status_updates_total` | counter, `MimicWorkload` status applies | `result` (`ok`, `error`) |
| `mimic_ui_requests_total` | counter (F6), answers of the UI listener (§7.5) | `route` (route pattern or `other`), `code` |
| `mimic_ui_skipped_total` | counter, `/v1/workloads` entries left out of the list (§7.5) | `reason` (`meta-unreadable`: the `:meta` of the deployment does not parse; also logged) |
| `mimic_ui_render_duration_seconds` | histogram (F6), parse and render of a stored report for `endpoints`, `openapi` and a document; buckets as `mimic_store_write_duration_seconds` | `route` |
| `mimic_sidecar_api_egress_builds_total` | counter (F9, controller; the sidecar's pull metrics are `mimic_egress_*`), egress projections built (§7.7); `too_large` also counts a pull refused `507` before its build | `result` (`ok`, `too_large`, `unreadable`, `error`) |
| `mimic_sidecar_api_egress_build_duration_seconds` | histogram (F9, controller), read, check, projection and serialisation of one build; buckets as `mimic_store_write_duration_seconds` | — |
| `mimic_sidecar_api_egress_cache_bytes` | gauge (F9, controller), bytes of `MIMIC_SIDECAR_API_EGRESS_BUDGET` held by projections, cached or being sent (a build refused by this budget is still counted `ok` in `mimic_sidecar_api_egress_builds_total`) | — |
| `mimic_load_runs_total` | counter (F11, controller), load test runs ended (§7.8); a run that fails before its admission is counted by every replica that writes its status, so two replicas can count it twice in a rare race (an admitted run is counted once, by the replica whose end script wins) | `phase` (`Succeeded`, `Failed`, `Aborted`), `reason` |
| `mimic_load_active_runs` | gauge (F11, controller), members of `mimic:v1:loads:active` at the last status pass | — |

  `mimic-load` (F11) exposes on its admin port, besides the common metrics:
  `mimic_load_requests_total{endpoint,result}` (`result`: `2xx` … `5xx` or an error kind of
  §7.8), `mimic_load_request_duration_seconds{endpoint}` (histogram, buckets as
  `mimic_http_request_duration_seconds`), `mimic_load_in_flight`, `mimic_load_missed_total`
  and `mimic_load_targets` (verified targets). `endpoint` is `<METHOD> <template>` of the plan,
  at most `MIMIC_LOAD_MAX_ENDPOINTS` values; one process serves one run.

  **Dashboard metrics** (#385). They add no `MIMIC_*` variable, and no label value comes from
  traffic, a Kubernetes object or a secret: every label below is a fixed enumeration.

  | Metric | Component | Type, help text and meaning | Labels |
  |---|---|---|---|
  | `process_cpu_seconds_total` | all with an admin listener (controller, sidecar, `mimic-load`) | counter, "Total user and system CPU time spent in seconds" | — |
  | `process_resident_memory_bytes` | same | gauge, "Resident memory size in bytes" | — |
  | `process_open_fds` | same | gauge, "Number of open file descriptors" | — |
  | `process_max_fds` | same | gauge, "Maximum number of open file descriptors" (the soft `RLIMIT_NOFILE`) | — |
  | `process_start_time_seconds` | same | gauge, "Start time of the process since unix epoch in seconds" | — |
  | `mimic_webhook_admissions_total` | controller (F1, INJ-09) | counter, "Pod admissions by result and reason" | `result` (`injected`, `skipped`, `error`), `reason`: with `injected` `ok`; with `skipped` the skip reasons of §7.1 (`host-network`, `unsupported-owner`, `port-conflict`, `uid-conflict`, `name-conflict`) and the untouched ones (`excluded-namespace`, `pod-disabled`, `not-selected`, `already-injected`); with `error` `too-large`, `unreadable`, `malformed`, `cache-not-synced`, `seed-not-loaded`, `ca-not-loaded`, `root`, `patch` |
  | `mimic_webhook_certificate_expiry_timestamp_seconds` | controller (F1, INJ-11) | gauge, "Expiry of the webhook serving certificate, in seconds since the Unix epoch": `notAfter` of the loaded serving certificate, 0 before one is loaded | — |
  | `mimic_webhook_admission_duration_seconds` | controller | histogram, "Time to answer one admission request": from the request head read on `/inject/*` to the response handed to the HTTP codec (body read, decision, patch), for every answered request, `4xx` included; the TLS handshake is not included; buckets 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10 | — |
  | `mimic_store_deployments` | controller | gauge, "Deployments with data in the store at the last status pass": members of `mimic:v1:workloads` (all namespaces) counted by each status pass (§7.4), 0 before the first | — |
  | `mimic_limit` | controller, sidecar | gauge, "Configured limit, by name": the parsed value of a `MIMIC_*` variable (sizes in bytes), set at start, constant for the process; `name` is the variable without `MIMIC_`, lower case, with `_bytes` added to sizes | `name`: controller `webhook_max_connections`, `sidecar_api_rate` (requests per second per source), `sidecar_api_max_report_bytes`, `sidecar_api_body_budget_bytes`, `sidecar_api_max_egress_bytes`, `sidecar_api_egress_budget_bytes`, `store_max_deployments` (per namespace), `store_max_endpoints`, `store_max_push_endpoints`, `store_max_literals`, `store_max_authorities`, `store_sample_budget_bytes`, `load_max_runs` (0: disabled), `load_max_namespace_runs`, `load_max_pending`, `load_max_concurrency`, `load_max_rate`, `load_max_endpoints`, `load_pod_concurrency`, `load_max_plan_bytes`, `load_store_budget_bytes` (the `MIMIC_LOAD_*` durations are not limits of this list); sidecar `max_connections` (per direction), `metrics_max_authorities`, `discovery_max_endpoints` (per direction), `discovery_max_authorities`, `discovery_sample_budget_bytes`, `discovery_capture_budget_bytes`, `discovery_queue`, `egress_max_bytes`, `mock_max_pending` |
  | `mimic_proxy_active_connections` | sidecar | gauge, "Client connections open": connections accepted on the direction's listeners and not closed yet, the count `MIMIC_MAX_CONNECTIONS` bounds (§4.1), tunnels included | `direction` |
  | `mimic_proxy_overhead_seconds` | sidecar | histogram, "Time spent in the sidecar per HTTP exchange, upstream wait excluded": from the request head handed over by the client codec to the request handed to the upstream connection, plus from the response head handed over by the upstream connection to the response handed to the client codec; in mock, from the request handed over (with the body bytes the match reads, §8) to the answer handed back, minus the simulated latency. Observed for every exchange answered with a response head (by the destination or the mock, `599` included); tunnels, `unframed` and `rejected` exchanges are not. Connection setup (connect, TLS handshakes), body streaming and socket writes are not included. Buckets 0.0001, 0.00025, 0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1 | `direction` |
  | `mimic_discovery_last_new_endpoint_timestamp_seconds` | sidecar | gauge, "Time an endpoint was last added to the template tree": time of the last recorded exchange whose endpoint (method and template; outbound, also its authority scope, §6.1) was not in the tree before it, a template made by absorption (§6) included; 0 before. The tree shape survives pushes (§7.2), so an endpoint seen again after a push is not new | `direction` |
  | `mimic_mock_mode_switches_total` | sidecar (F10) | counter, "Mode changes applied": +1 each time the sidecar applies a mode other than the one it had (§8 "Mode state"); the mode taken at start, a change of factor only are not counted; a switch counts when applied, whether or not the state file could be written (§8, MCK-10) | `to` (`mock`, `discovery`) |
  | `mimic_mock_latency_factor` | sidecar (F10) | gauge, "Latency factor held": the factor of §8 as a decimal (hundredths / 100, e.g. 1.5), set at start and at each change, in both modes (it applies only in mock) | — |
  | `mimic_load_planned_rate` | `mimic-load` (F11) | gauge, "Requests per second the schedule asks for now": in the open model, the pod's rate × elapsed / `rampUp` during `rampUp`, the pod's rate after it until `duration`, 0 before the pod's schedule starts and after `duration` (`load::schedule`, computed at each scrape from the elapsed monotonic time); the sum over a run's pods is the run's planned rate. Not exposed in the closed model (no rate is planned) | — |

  The `process_*` names and help texts are those of the Prometheus client libraries, without the
  `mimic_` prefix and without labels. They are read from `/proc/self/stat`, `/proc/self/limits`,
  `/proc/self/fd` and the `btime` of `/proc/stat` at each scrape, never from `environ`,
  `cmdline` or memory maps; outside Linux, or when a read fails, the family is left out of that
  scrape (one DEBUG line `process metrics unavailable` with `file`). `mimic_limit` and
  `mimic_store_deployments` hold the same value on every controller replica: dashboards read
  them with `max`, like `mimic_load_active_runs`.

  **Exporter metrics used** (#378, #384): the Valkey dashboard reads only these series of the
  Valkey exporter, so that the dashboard check can verify every name: `redis_up`,
  `redis_uptime_in_seconds`, `redis_instance_info`, `redis_memory_used_bytes`,
  `redis_memory_max_bytes`, `redis_mem_fragmentation_ratio`, `redis_evicted_keys_total`,
  `redis_expired_keys_total`, `redis_commands_processed_total`,
  `redis_commands_duration_seconds_total{cmd}`, `redis_connected_clients`,
  `redis_rejected_connections_total`, `redis_db_keys{db}`, `redis_aof_last_write_status`.
  `redis_db_keys` is the total of the database (mimic is its only user); per-prefix counts would
  need `--check-keys`, which puts key names into labels, and are not used.

  `authority` is the normalised target authority of §4.1 (lower case, no trailing dot, default
  port removed, `(invalid)` when it is not a host name or IP literal; an HTTP/1 `Host` value or
  request-target authority is first cut to 1024 bytes, the codec's memory bound). Per direction and per
  metric, the first `MIMIC_METRICS_MAX_AUTHORITIES` distinct values are kept; later ones are
  counted under `authority="(other)"`. The two sentinels contain characters that no
  normalised authority has, so a real host named `other` or `invalid` keeps its own series,
  and they never take a budget slot. The set restarts empty with the process.
  `mimic_http_request_duration_seconds` observes the exchanges whose response head was read;
  `unframed` and `rejected` exchanges are counted in `mimic_http_requests_total` only.
- **Valkey exporter** (#378, §10.7): with `valkey.metrics.enabled` the Valkey pod's exporter
  (`oliver006/redis_exporter`, port 9121) reads `INFO` only, as the ACL user `mimic-exporter`
  (`+info +ping`, no key access, §11). It exposes no key, value or client list. The dashboards use
  only the series listed under "Exporter metrics used" above.
- **Aggregation** (#378): the controller's series are summed across its replicas (there is no
  leader), except the gauges set by a status pass (`mimic_load_active_runs`), which are read with
  `max`.
- **Health**: `/healthz` (process alive) and `/readyz` (ready to serve) on the admin port.
- No traces.

## 10. Configuration and runtime (12-factor)

- Configuration comes **only** from environment variables prefixed `MIMIC_`. An invalid value
  stops the process at start with exit code **2** and a message naming the variable.
- Processes are stateless (state is in Valkey or in memory), log to stdout, stop gracefully on
  `SIGTERM`, and bind their ports from configuration.
- Durations are written `<n>ms`, `<n>s` and, from F5, `<n>m`, `<n>h` or `<n>d`.
- One image `ghcr.io/thaliph/mimic` (distroless, non-root UID 65532, no shell) holds
  `mimic-sidecar` and `mimic-controller` (the webhook runs the sidecar as UID 1337).
  From F3 the `mimic` image also holds the static `mimic-trust` binary (§7.1). From F6 the
  `mimic-controller` binary embeds the UI assets, Swagger UI included (§7.5): the image gains no
  file and the UI fetches nothing from outside the listener. From F11 it also holds
  `mimic-load` (§7.8), run by load test Jobs as UID 65532.
  `ghcr.io/thaliph/mimic-init` (distroless, no shell) holds only `mimic-init`, `iptables-restore`
  and `ip6tables-restore` (nft backend) and their libraries.
- `mimic-init` has no admin listener (it exits): FND-02 and FND-03 do not apply to it.
- Installation through the Helm chart `oci://ghcr.io/thaliph/charts/mimic`.
- Release artefacts are multi-arch (amd64, arm64), signed with cosign (keyless), with SBOM and
  provenance.

### 10.1 Common variables

| Variable | Default | Meaning |
|---|---|---|
| `MIMIC_LOG` | `info` | Log filter (`tracing` syntax). |
| `MIMIC_ADMIN_ADDR` | sidecar `0.0.0.0:15100`, controller `0.0.0.0:8081` | Admin listener: `/healthz`, `/readyz`, `/metrics`. |
| `MIMIC_SHUTDOWN_GRACE` | `10s` | Maximum drain time after `SIGTERM` (`<n>s` or `<n>ms`); the sidecar then closes the connections still open (§4.1). |

### 10.2 Controller variables

| Variable | Default | Meaning |
|---|---|---|
| `MIMIC_VALKEY_URL` | required | `redis://host:port[/db]`, without credentials. |
| `MIMIC_VALKEY_PASSWORD` | empty | Valkey password (from a Secret). |
| `MIMIC_NAMESPACE` | required | Controller namespace (downward API). |
| `MIMIC_WEBHOOK_ADDR` | `0.0.0.0:8443` | Webhook HTTPS listener. |
| `MIMIC_WEBHOOK_SERVICE` | required | Service name of the webhook (certificate SAN). |
| `MIMIC_WEBHOOK_SECRET` | required | Secret holding the webhook CA and serving certificate. |
| `MIMIC_WEBHOOK_MAX_CONNECTIONS` | `256` | Concurrent webhook connections served (1–65535). |
| `MIMIC_WEBHOOK_REQUEST_TIMEOUT` | `5s` | TLS handshake and per-request read deadline (`<n>s` or `<n>ms`, > 0). |
| `MIMIC_WEBHOOK_CONFIGURATION` | required | Name of the `MutatingWebhookConfiguration` whose `caBundle` the controller maintains. |
| `MIMIC_INJECT_SIDECAR_IMAGE` | required | Image of `mimic-sidecar` in injected pods (digest recommended); the shared `mimic` image, so the injected container sets `command: [/usr/local/bin/mimic-sidecar]`. |
| `MIMIC_INJECT_INIT_IMAGE` | required | Image of `mimic-init` in injected pods. |
| `MIMIC_INJECT_SIDECAR_RESOURCES` | `{"requests":{"cpu":"50m","memory":"64Mi"},"limits":{"memory":"128Mi"}}` | Sidecar `resources`, JSON. |
| `MIMIC_INJECT_SIDECAR_LOG` | `info` | `MIMIC_LOG` given to injected sidecars. |
| `MIMIC_TLS_CA_SECRET` | required | Secret of the controller's namespace holding the TLS seed (§5). |
| `MIMIC_SIDECAR_API_ADDR` | `0.0.0.0:8444` | Sidecar API HTTPS listener (§7.2); its port is also the one given to sidecars in `MIMIC_CONTROLLER_URL`. |
| `MIMIC_SIDECAR_API_RATE` | `10` | Requests per second per source address on the sidecar API, burst twice that (§7.2), 1–10000. |
| `MIMIC_SIDECAR_API_MAX_REPORT` | `32Mi` | Largest push body (`<n>`, `<n>Ki`, `<n>Mi`; 64Ki–1Gi). |
| `MIMIC_SIDECAR_API_BODY_BUDGET` | `32Mi` | Push bodies being read at once, in bytes (same syntax; at least `MIMIC_SIDECAR_API_MAX_REPORT`, at most 4Gi); from F9 egress builds hold a share too (§7.7). |
| `MIMIC_SIDECAR_API_MAX_EGRESS` | `4Mi` | (F9) Largest egress set answered (§7.7; same syntax, 64Ki–1Gi); a larger one gets `507`. Keep it at most the sidecars' `MIMIC_EGRESS_MAX_BYTES`. |
| `MIMIC_SIDECAR_API_EGRESS_BUDGET` | `32Mi` | (F9) Bytes of egress sets cached or being sent at once per replica (§7.7; same syntax, at least `MIMIC_SIDECAR_API_MAX_EGRESS`, at most 4Gi). |
| `MIMIC_STORE_TTL` | `30d` | Retention of a deployment's data after its last push, and of endpoints and samples after they were last seen (§7.3); 10s–365d. |
| `MIMIC_STORE_MAX_ENDPOINTS` | `512` | Endpoints per direction and deployment in the store, 1–10000. |
| `MIMIC_STORE_MAX_PUSH_ENDPOINTS` | `256` | Endpoints per direction in one pushed report (§7.2), 1–10000, at most `MIMIC_STORE_MAX_ENDPOINTS`. |
| `MIMIC_STORE_MAX_AUTHORITIES` | `128` | (F8) Outbound scopes (authorities) per deployment in the store and in one push (§7.3), 1–10000. |
| `MIMIC_STORE_MAX_LITERALS` | `32` | Literal limit of the store's templating (§6), 2–1000. |
| `MIMIC_STORE_SAMPLE_BUDGET` | `4Mi` | Sample bytes per deployment in the store (1Mi–1Gi). |
| `MIMIC_STORE_MAX_DEPLOYMENTS` | `50` | Deployments with data per namespace (§7.3), 1–100000. |
| `MIMIC_STATUS_INTERVAL` | `30s` | Period of the `MimicWorkload` status update (§7.4), 1s–1h. |
| `MIMIC_STATUS_POD_WINDOW` | `5m` | A pod counts in `status.discovery.pods` while its last push is this recent, 1s–24h. |
| `MIMIC_LOAD_MAX_RUNS` | `0` | (F11) Load test runs active at once in the cluster (§7.8), 0–100; `0` disables load tests. The chart sets `4` when `loadTest.enabled`. |
| `MIMIC_LOAD_MAX_NAMESPACE_RUNS` | `2` | (F11) Active runs per namespace, 1–100, at most `MIMIC_LOAD_MAX_RUNS` when that is not 0. |
| `MIMIC_LOAD_MAX_PENDING` | `16` | (F11) `Pending` runs per namespace; newer ones fail `TooManyPending`, 1–1000. |
| `MIMIC_LOAD_STORE_BUDGET` | `64Mi` | (F11) Valkey bytes reserved by active runs together (plans, per-pod results, summaries), `<n>`, `<n>Ki`, `<n>Mi`, `<n>Gi`, 1Mi–4Gi, at least `MIMIC_LOAD_MAX_PLAN` + 1Mi; a run that does not fit fails `StoreFull`. The reservation counts payload bytes: the memory Valkey really uses can exceed the budget by up to 25% (allocator size classes). |
| `MIMIC_LOAD_MAX_CONCURRENCY` | `256` | (F11) Highest `spec.concurrency` accepted, 1–4096. |
| `MIMIC_LOAD_MAX_RATE` | `2000` | (F11) Highest `spec.rate` accepted, requests per second, 1–100000. |
| `MIMIC_LOAD_MAX_DURATION` | `30m` | (F11) Longest `spec.duration` accepted, 10s–24h. |
| `MIMIC_LOAD_POD_CONCURRENCY` | `128` | (F11) Concurrency of one generator pod: a run gets `ceil(concurrency / this)` pods, 1–4096. |
| `MIMIC_LOAD_MAX_ENDPOINTS` | `64` | (F11) Endpoints in a plan (§7.8), 1–256. |
| `MIMIC_LOAD_MAX_PLAN` | `8Mi` | (F11) Largest serialised plan (`<n>`, `<n>Ki`, `<n>Mi`; 64Ki–256Mi). The floor leaves room for the plan's skeleton (about 34 bytes without endpoint) so that a shrunk plan is always readable; a smaller value is invalid (exit 2, like any invalid `MIMIC_*` value). `load::plan` itself never builds a plan over a limit below 64Ki: it uses 64Ki. |
| `MIMIC_LOAD_START_TIMEOUT` | `5m` | (F11) Longest wait in `Pending` for the gate, and for a verified target (§7.8), 5s–1h. |
| `MIMIC_LOAD_RESOURCES` | `{"requests":{"cpu":"500m","memory":"128Mi"},"limits":{"cpu":"2","memory":"256Mi"}}` | (F11) Generator pod `resources`, JSON. |
| `MIMIC_LOAD_NETWORK_POLICY` | absent | (F11) JSON `{"controllerLabels": {…}, "dnsNamespace": "…", "dnsLabels": {…}, "metricsFrom": […]}`: when set, each run also gets the NetworkPolicy of §7.8 built from these peers (the controller pods' labels, the DNS pods' namespace and labels, the NetworkPolicy peers allowed on the admin port: the chart's effective `loadTest.metricsFrom` of §10.7, which, when empty and `metrics.podMonitor.enabled`, falls back to the namespace peer `networkPolicy.metricsNamespaceSelector`; the admission policy checks the same value); absent: no NetworkPolicy. The chart sets it when `loadTest.networkPolicy`. |
| `MIMIC_UI_ADDR` | `127.0.0.1:8080` | Read API (§7.5) and, from F6, UI listener (pages, OpenAPI documents, embedded Swagger UI); plain HTTP, loopback peers only. |

### 10.3 Sidecar variables

Set by the webhook; the defaults are the values it sets.

| Variable | Default | Meaning |
|---|---|---|
| `MIMIC_INBOUND_ADDRS` | `0.0.0.0:15006,[::]:15006` | Inbound listeners. |
| `MIMIC_OUTBOUND_ADDRS` | `127.0.0.1:15001,[::1]:15001` | Outbound listeners, loopback only. |
| `MIMIC_WORKLOAD` | required | Deployment name. |
| `MIMIC_POD_NAMESPACE`, `MIMIC_POD_NAME`, `MIMIC_POD_IP` | required | Downward API. |
| `MIMIC_APP_PROBES` | `{}` | Original `httpGet` probes, JSON object keyed by `<container>/<livez\|readyz\|startupz>`. |
| `MIMIC_MAX_CONNECTIONS` | `16384` | Connections served at once in each direction, inbound and outbound separately (§4.1), 1–1000000. |
| `MIMIC_METRICS_MAX_AUTHORITIES` | `50` | Distinct `authority` label values kept per direction and metric (§9), 1–10000. Not set by the webhook in F2. |
| `MIMIC_TLS_CA` | empty | Deployment root certificate (PEM). Empty: no interception, every outbound TLS connection is passed through (`no_ca`), with one INFO line at start. |
| `MIMIC_TLS_EXCLUDE` | empty | Comma-separated exclusions (§5); an invalid entry or more than 256 exits with code 2. |
| `MIMIC_CONTROLLER_URL` | required when `MIMIC_TLS_CA` is set | Sidecar API base URL (`https://…`). Without it, discovery reports are not shipped (§7.2). |
| `MIMIC_CONTROLLER_CA` | required when `MIMIC_TLS_CA` is set | CA (PEM) the sidecar API certificate is verified against. |
| `MIMIC_TLS_TOKEN_FILE` | `/var/run/secrets/mimic.platform.io/token` | Projected token, re-read for every sidecar API request. |
| `MIMIC_TRUST_BINARY` | `/usr/local/bin/mimic-trust` | The `mimic-trust` binary published into the trust volume (tests point it elsewhere); the webhook never sets it. |
| `MIMIC_TRUST_DIR` | `/mimic` | `mimic-trust` volume: binary and fallback store written at start, `trust/*/image-ca.pem` read as upstream roots. |
| `MIMIC_DISCOVERY_MAX_ENDPOINTS` | `256` | Endpoints kept per direction (§6), each direction on its own (§6.1), 1–10000. Not set by the webhook. |
| `MIMIC_DISCOVERY_MAX_AUTHORITIES` | `64` | Outbound scopes (authorities) kept (§6.1), 1–10000. Not set by the webhook. |
| `MIMIC_DISCOVERY_MAX_LITERALS` | `32` | Distinct literal segments below a template node before it absorbs them into a parameter (§6), 2–1000. Not set by the webhook. |
| `MIMIC_DISCOVERY_SAMPLE_BUDGET` | `16Mi` | Bytes held by samples (`<n>`, `<n>Ki`, `<n>Mi`; 1Mi–1Gi). Not set by the webhook. |
| `MIMIC_DISCOVERY_CAPTURE_BUDGET` | `8Mi` | Bytes of bodies captured in flight and queued (same syntax; 1Mi–1Gi). Not set by the webhook. |
| `MIMIC_DISCOVERY_QUEUE` | `1024` | Exchange records waiting for the recorder (1–65536). Not set by the webhook. |
| `MIMIC_DISCOVERY_PUSH_INTERVAL` | `30s` | Period of the discovery push (§6, §7.2), 1s–1h. Not set by the webhook. |
| `MIMIC_DISCOVERY_REDACT_NAMES` | empty | Extra names redacted by name (§6), comma-separated, at most 64, each 1–64 characters of `a-z0-9-_` (compared like the built-in names); the webhook sets the sorted, deduplicated union, lower-cased, of `spec.discovery.redact` of every `MimicWorkload` naming the deployment. |
| `MIMIC_EGRESS_PULL_INTERVAL` | `60s` | (F9) Period of the egress pull (§7.7), 1s–1h. Not set by the webhook. |
| `MIMIC_EGRESS_MAX_BYTES` | `4Mi` | (F9) Largest egress set read from the controller (§7.7; `<n>`, `<n>Ki`, `<n>Mi`, `<n>Gi`; 64Ki–1Gi). Not set by the webhook. Sized so that the pull peak and the discovery worst case fit the 128 Mi sidecar limit (PUL-18): raising it needs a larger `MIMIC_INJECT_SIDECAR_RESOURCES` memory limit. |
| `MIMIC_MODE` | `discovery` | (F10) Mode until the first pull answer (§3, §8): `discovery` or `mock`. The webhook always sets it to the effective mode at admission (§7). |
| `MIMIC_MOCK_LATENCY_FACTOR` | `1` | (F10) Latency factor until the first pull answer (§8), a decimal 0–100 with at most 2 fractional digits. Set by the webhook when the effective factor is not 1. |
| `MIMIC_MOCK_MAX_PENDING` | `1024` | (F10) Mocked requests pending at once (§8 "Bounds"), 1–65536; more are answered `599` (`overload`). Not set by the webhook. Sized with the 128 Mi sidecar limit (MCK-18): raising it needs a larger `MIMIC_INJECT_SIDECAR_RESOURCES` memory limit. |
| `MIMIC_MOCK_PASSTHROUGH` | empty | (F10) Destinations handled as in discovery while in mock (§8), comma-separated, the `MIMIC_TLS_EXCLUDE` grammar; an invalid entry or more than 256 exits with code 2. The webhook sets the union of `spec.mock.passthrough` (§7), absent when empty. |
| `MIMIC_LOAD_ENABLED` | absent | (F11) `1`: the sidecar honours the load marker of inbound requests and answers `GET /load` (§4.1, §7.8). The webhook sets it when `MIMIC_LOAD_MAX_RUNS` is above 0 (chart `loadTest.enabled`); absent otherwise. |

### 10.4 `mimic-init` variables

| Variable | Default | Meaning |
|---|---|---|
| `MIMIC_INBOUND_PORT` | `15006` | Redirection target for inbound TCP. |
| `MIMIC_OUTBOUND_PORT` | `15001` | Redirection target for outbound TCP. |
| `MIMIC_PROXY_UID` | `1337` | UID whose traffic is not redirected. |
| `MIMIC_EXCLUDE_INBOUND_PORTS` | empty | Comma-separated ports (1–65535) never redirected inbound. |

### 10.5 `mimic-trust` variables

| Variable | Default | Meaning |
|---|---|---|
| `MIMIC_TRUST_CA` | required | Deployment root certificate (PEM). |
| `MIMIC_TRUST_OUT` | required | Output directory (created if missing). |
| `MIMIC_TRUST_FALLBACK` | `/mimic/bin/fallback-ca.pem` | Store used when the image has none. |

`mimic-trust` has no admin listener (it exits): FND-02 and FND-03 do not apply to it. It is a
static binary (no dynamic loader, no libc needed) so that it runs in any Linux app image.

### 10.6 `mimic-load` variables (F11)

Set by the controller in the Job (§7.8) as literal values only; the defaults are the values
it sets. The pod's index and the Job's pod count come from the load API (§7.8).

| Variable | Default | Meaning |
|---|---|---|
| `MIMIC_LOAD_RUN` | required | `<namespace>/<name>/<uid>` of the `MimicLoadTest`. |
| `MIMIC_LOAD_DEPLOYMENT` | required | Deployment under test (DNS-1123). |
| `MIMIC_CONTROLLER_URL`, `MIMIC_CONTROLLER_CA` | required | Load API base URL and its CA, as for sidecars (§10.3). |
| `MIMIC_LOAD_TOKEN_FILE` | `/var/run/secrets/mimic.platform.io/load-token` | Projected token (audience `mimic.platform.io/load`), re-read for every request. |
| `MIMIC_LOAD_CREDENTIALS_DIR` | `/mimic/credentials` | Mounted credentials Secret; absent when the spec has none. |
| `MIMIC_LOAD_REPORT_INTERVAL` | `5s` | Period of the targets poll and the results post, 1s–1m. |
| `MIMIC_LOAD_MAX_PLAN` | `8Mi` | Largest plan read (same syntax as the controller's). |
| `MIMIC_LOAD_PROBE_PORT` | `15100` | The targets' admin port, where the probe of §7.8 is sent. The Job never sets it (the admission policy lists the allowed names); it is for tests. |

`mimic-load` has the common variables (§10.1) and admin listener (`0.0.0.0:15100`).

### 10.7 Monitoring values (Helm, #378, #379)

Chart values of the scrape objects, all off by default. Nothing of them is rendered unless enabled,
so the default render is the one of §12.13 (MON-01).

| Value | Default | Meaning |
|---|---|---|
| `valkey.metrics.enabled` | `false` | The Redis exporter as a container of the Valkey pod, with the Secret `<release>-valkey-exporter` (its password, kept on upgrade; or `valkey.metrics.existingSecret`), and the init container `valkey-acl` builds the ACL file at each pod start (§10.7). Enabling restarts Valkey once; with `valkey.persistence.enabled: false` the store is lost on that restart. |
| `valkey.metrics.image` | `docker.io/oliver006/redis_exporter:v1.95.0@sha256:afb2…` | Pinned by digest in `images/Dockerfile` (stage `redis-exporter`, checked by `scripts/check-pins.sh`). Released 2026-10-21, outside the 14-day cooldown. |
| `valkey.metrics.resources` | requests 10m CPU and 16Mi memory, limit 32Mi memory | Resources of the exporter container. |
| `valkey.metrics.existingSecret` | `""` | Secret holding the exporter's password under the key `password`, for GitOps users (#399). Set: the chart renders no `<release>-valkey-exporter` Secret, and the exporter and the init container `valkey-acl` read this one. Empty: the chart's own Secret, kept by `lookup` (below). |
| `metrics.podMonitor.enabled` | `false` | Prometheus Operator PodMonitors, the admin port by its name `mimic-admin` (15100): `<release>-sidecar` (job `mimic-sidecar`) of the injected pods, selected by `mimic.platform.io/deployment` Exists, with the target labels `namespace`, `pod` and `deployment`; `<release>-load` (job `mimic-load`) of the generator pods, selected by `mimic.platform.io/load-test` Exists, with the target label `load_test`; and, when `valkey.metrics.enabled`, `<release>-valkey` (job `mimic-valkey`, port `metrics`, 9121) of the Valkey pods in the release namespace. Needs the `monitoring.coreos.com` CRDs at install; the bundled Prometheus of #379 does not read it. No NetworkPolicy is added for the sidecar port (§11.1). |
| `metrics.podMonitor.interval` | `30s` | Scrape interval of the PodMonitors. |
| `metrics.podMonitor.namespaceSelector` | `{any: true}` | Namespaces the PodMonitors select pods in: `any: true`, or `matchNames` / `matchLabels`. The default is every namespace, since a pod can be injected through a `MimicWorkload` alone. The Prometheus that reads the PodMonitors needs `list` and `watch` on `pods` in each namespace with injected pods (a ClusterRole, or a Role per namespace): without it, that namespace has no target, and Prometheus logs forbidden errors (#399). Set `matchNames` to narrow the default. |
| `loadTest.metricsFrom` | `[]` | NetworkPolicy peers that may scrape a generator on 15100. Empty while `metrics.podMonitor.enabled` or `monitoring.prometheus.enabled` (#379) is true: the namespace peer `networkPolicy.metricsNamespaceSelector` (the release namespace when that is empty). A set value wins. |

The exporter's arguments are fixed: `--web.listen-address=0.0.0.0:9121` and
`--redis.addr=redis://localhost:6379`. Its env holds `REDIS_USER` (`mimic-exporter`) and
`REDIS_PASSWORD` (a `secretKeyRef`) and nothing else. No flag reads keys, values or client lists
(`--check-keys`, `--check-single-keys`, `--check-streams`, `--script`, `--export-client-list` and
`--include-config-metrics` are never given). The exporter reaches Valkey on `localhost`, so the
Valkey port needs no new ingress rule. The exporter has a `livenessProbe` (TCP on `metrics`) and no
`readinessProbe` (#399): a container's readiness feeds the pod's, so a restart of the exporter would
take the Valkey Service endpoint away.

With `valkey.metrics.enabled` the Valkey pod gets `--aclfile` and no `--requirepass`: the file is its
only source of users, and Valkey recreates `default` as `on nopass` when the file lacks it (the store
open, the controller's AUTH failing; #378). An init container `valkey-acl` (the pinned Valkey image, the
restricted security context of the valkey container, a read-only root) builds the file at each pod start
from the Secrets, into an in-memory emptyDir mounted read-only in Valkey, mode 0440. It holds two lines
and SHA-256 hashes only: `user default on #<sha256 of VALKEY_PASSWORD> ~* &* +@all`, where VALKEY_PASSWORD
is the Valkey password of `valkey.existingSecret` or of Secret `<release>-valkey`, and
`user mimic-exporter on #<sha256 of EXPORTER_PASSWORD> +info +ping resetkeys resetchannels`, where
EXPORTER_PASSWORD is the password of Secret `valkey.metrics.existingSecret` or else of Secret `<release>-valkey-exporter`. The init container exits non-zero
when a password is empty and never prints one. No line says `nopass`. No Secret holds the file, so a
render needs no lookup for it, and the file follows `valkey.existingSecret` and a rotated Secret after a
restart of the Valkey pod. This settles item 2 of #399 for the default user. With `valkey.metrics.enabled`
false the default user keeps `--requirepass` and there is no ACL file.

The exporter's password is the Secret `<release>-valkey-exporter`, kept by a `lookup` as the Valkey password
is. A render without a cluster (`helm template`, Argo CD) finds no Secret and draws a new password at each
render. Valkey reads its ACL file at start and never reloads it, so after such a render the exporter's next
restart fails its AUTH until Valkey restarts. The chart never restarts Valkey on a render: the StatefulSet
carries no checksum of the exporter Secret, and the exporter reads the password by `secretKeyRef`. GitOps users
set `valkey.metrics.existingSecret` to a Secret they own (#399): the chart then renders no exporter Secret, and
the exporter and `valkey-acl` read that key. To rotate it, change that Secret and restart Valkey (the ACL file
is built at pod start), then the exporter.

#### Bundled Prometheus and Grafana (#379)

For a demo or a dev cluster only: a production install brings its own monitoring and uses the
ServiceMonitor, the PodMonitors and the dashboard ConfigMaps. Both are off by default, and with both
off the render is the one of MON-01 (MON-14). Both are rendered by the chart's own templates, with no
Helm dependency: the chart has no `dependencies`, no `Chart.lock` and no `charts/`, so a render needs no
`helm dependency build` and the published chart vendors no third-party archive. Alerting, long-term
storage, ingress and the dashboards themselves (#380 to #384) are out of scope. The bundled Prometheus does not scrape the kubelet or cAdvisor, which needs
`nodes/proxy`; resource use comes from the `process_*` series of mimic (#385).

| Value | Default | Meaning |
|---|---|---|
| `monitoring.prometheus.enabled` | `false` | Renders `<release>-prometheus`: a Deployment (1 replica, `Recreate`), a Service `ClusterIP` on 9090 only, a ConfigMap `prometheus.yml`, a ServiceAccount, its RBAC (below) and, with `networkPolicy.enabled`, a NetworkPolicy (§11). |
| `monitoring.prometheus.image` | `quay.io/prometheus/prometheus:v3.15.1@sha256:fa33…` | Pinned by digest in `images/Dockerfile` (stage `prometheus`, checked by `scripts/check-pins.sh`). Built 2026-10-21, outside the 14-day cooldown. |
| `monitoring.prometheus.retention` | `24h` | `--storage.tsdb.retention.time`. |
| `monitoring.prometheus.retentionSize` | `800MB` | `--storage.tsdb.retention.size`, below the emptyDir limit so that the kubelet never evicts the pod for its data. |
| `monitoring.prometheus.storageSizeLimit` | `1Gi` | `sizeLimit` of the `emptyDir` that holds the TSDB. There is no persistent volume: the data are lost when the pod restarts. |
| `monitoring.prometheus.resources` | requests 100m CPU and 256Mi memory, limit 512Mi memory | Resources of the Prometheus container. |
| `monitoring.prometheus.namespaces` | `[]` | Namespaces of the sidecar and load jobs. Empty: every namespace, through a ClusterRole. A list: those namespaces and the release namespace, through a Role and a RoleBinding in each (each namespace must exist at install). |
| `monitoring.grafana.enabled` | `false` | Renders `<release>-grafana`: a Deployment (1 replica, `Recreate`) with the containers `grafana` and `dashboards` (the k8s-sidecar), a Service `ClusterIP` on 3000 only, the ConfigMaps `<release>-grafana` (`grafana.ini`) and `<release>-grafana-provisioning` (the datasource and the dashboards provider), the Secret `<release>-grafana-admin`, a ServiceAccount, the Role and RoleBinding `<release>-grafana-dashboards` and, with `networkPolicy.enabled`, a NetworkPolicy (§11). Grafana can be enabled without the bundled Prometheus (see `datasourceUrl`). |
| `monitoring.grafana.image` | `docker.io/grafana/grafana:13.2.4-distroless@sha256:ae3d…` | Pinned by digest in `images/Dockerfile` (stage `grafana`, checked by `scripts/check-pins.sh`). Built 2026-10-12, outside the 14-day cooldown. |
| `monitoring.grafana.sidecarImage` | `quay.io/kiwigrid/k8s-sidecar:2.14.1@sha256:092d…` | The dashboard sidecar. Pinned by digest in `images/Dockerfile` (stage `k8s-sidecar`, checked by `scripts/check-pins.sh`). Built 2026-10-08. |
| `monitoring.grafana.datasourceUrl` | `""` | URL of the provisioned Prometheus datasource. Empty: `http://<release>-prometheus:9090`, the bundled Prometheus, also when it is off. An external datasource needs `datasourceEgress` as well, with `networkPolicy.enabled`. |
| `monitoring.grafana.datasourceEgress` | `[]` | NetworkPolicy egress rules appended to the Grafana NetworkPolicy, rendered only when `datasourceUrl` is set. Required for an external datasource with `networkPolicy.enabled`: the bundled policy admits only the bundled Prometheus, the DNS and the API server, so without a rule the datasource is unreachable. Each entry is a NetworkPolicy egress rule (`to`, `ports`), for example `{to: [{ipBlock: {cidr: 10.20.0.0/16}}], ports: [{port: 9090, protocol: TCP}]}`. Nothing is added when empty. |
| `monitoring.grafana.resources` | requests 50m CPU and 192Mi memory, limit 384Mi memory (about 180 MiB measured with the plugins of §10.7) | Resources of the `grafana` container. |
| `monitoring.grafana.sidecarResources` | requests 10m CPU and 64Mi memory, limit 128Mi memory | Resources of the `dashboards` container. Measured on k8s-sidecar 2.14.1 (Python client): 66 to 78 MiB after start with 8 dashboards of 60 KB, about 90 MiB steady with 30 of 150 KB; the former 64Mi limit was OOMKilled in a loop. |
| `networkPolicy.apiServerCIDR` | `""` | The `ipBlock` of the API server in the egress rules of Prometheus and Grafana (TCP 443 and 6443). Empty: any destination on those two ports, since the API server's address depends on the cluster. |

Prometheus runs with exactly the arguments `--config.file=/etc/prometheus/prometheus.yml`,
`--storage.tsdb.path=/prometheus`, `--storage.tsdb.retention.time=<retention>`,
`--storage.tsdb.retention.size=<retentionSize>`, `--web.listen-address=:9090` and `--log.format=json`:
no `--web.enable-admin-api`, `--web.enable-lifecycle`, `--web.enable-remote-write-receiver` or
`--web.enable-otlp-receiver`. A change of `prometheus.yml` rolls the pod through a checksum annotation,
since there is no reload endpoint. Its security context: `runAsUser` and `runAsGroup` 65534 (the image
names its user `nobody`, which the kubelet cannot check as non-root), `runAsNonRoot`, a read-only root
filesystem, no privilege escalation, `drop: [ALL]`, `RuntimeDefault`. The token is mounted, for service
discovery. Probes: `/-/ready` and `/-/healthy` on 9090.

`prometheus.yml` holds one job per scrape object of the chart, rendered from the same helper
(`mimic.scrapeTargets`) as the ServiceMonitor and the PodMonitors, so that the selector, the port, the
path, the interval and the relabelings cannot drift (MON-17). Every job uses `kubernetes_sd_configs` with
`role: pod` and a server-side label `selectors` entry, keeps the container port by name, and drops the
pods in phase `Succeeded` or `Failed`, as the Operator does:

| Job (`job` label) | Pods | Namespaces | Port | Target labels |
|---|---|---|---|---|
| `<release>-controller` (the Service name, the Operator's job name for the ServiceMonitor) | `app.kubernetes.io/instance=<release>`, `app.kubernetes.io/name=mimic-controller` | the release namespace | `admin` (8081) | `namespace`, `pod` |
| `mimic-sidecar` | `mimic.platform.io/deployment` Exists | `monitoring.prometheus.namespaces`, or all | `mimic-admin` (15100) | `namespace`, `pod`, `deployment` |
| `mimic-load` | `mimic.platform.io/load-test` Exists | as `mimic-sidecar` | `mimic-admin` | `namespace`, `pod`, `load_test` |
| `mimic-valkey`, only with `valkey.metrics.enabled` | the Valkey pods of the release | the release namespace | `metrics` (9121) | `namespace`, `pod` |

The intervals are `metrics.serviceMonitor.interval` (controller) and `metrics.podMonitor.interval` (the
others). No job scrapes Prometheus itself, a node, the kubelet or the API server, and no job carries
credentials (`authorization`, `basic_auth`, `bearer_token`, `bearer_token_file`, `oauth2`, `tls_config`).

Prometheus RBAC: `list` and `watch` on `pods` (core group) and nothing else, in a ClusterRole and a
ClusterRoleBinding `<release>-prometheus`, or a Role and a RoleBinding per namespace when
`monitoring.prometheus.namespaces` is set. The controller is discovered as pods, not through its
Service, so neither `services`, `endpoints` nor `endpointslices` is granted.

The Grafana Deployment:

- Admin: the Secret `<release>-grafana-admin` holds `admin-password`, 32 `randAlphaNum` characters, kept
  across upgrades by `lookup` as the Valkey Secret is (a render without a cluster draws a new one, §11.1).
  It is mounted read-only into the `grafana` container only, which reads it through `grafana.ini`:
  `security.admin_password = $__file{/etc/grafana-admin/admin-password}`. The user is `admin`
  (`security.admin_user`). The password is never a value, an argument, an env value or a log field, and
  the `grafana` container has no env at all. The env form `GF_SECURITY_ADMIN_PASSWORD__FILE` is not used:
  Grafana 13.2.4 logs it as an override, but the default admin is created with `admin` as password, so
  every login with the Secret answers `401 password-auth.invalid` (reproduced with the pinned image).
- `grafana.ini`: `analytics.reporting_enabled = false`, `analytics.check_for_updates = false`,
  `analytics.check_for_plugin_updates = false`, `news.news_feed_enabled = false`,
  `plugins.preinstall_disabled = true`, `plugins.disable_plugins` listing every bundled datasource plugin
  but `prometheus` (each runs as a process of about 30 MiB; the legacy `postgres` ID is listed too, #407),
  `auth.anonymous.enabled = false`, `security.disable_gravatar = true`, `users.allow_sign_up = false`,
  `log.mode = console`, `log.console.format = json`. No plugin is installed. Nothing in Grafana calls the
  internet.
- Provisioning: one datasource, `type: prometheus`, `uid: mimic-prometheus` (the dashboards of #380 to
  #384 refer to it), the URL of `datasourceUrl`, `access: proxy`, `isDefault: true`, `editable: false`; one
  dashboards provider of type `file` on `/tmp/dashboards`, `updateIntervalSeconds: 30`,
  `allowUiUpdates: false`.
- Dashboard sidecar (`dashboards` container, k8s-sidecar): env `LABEL=grafana_dashboard`, `LABEL_VALUE=1`,
  `RESOURCE=configmap`, `NAMESPACE=<release namespace>`, `METHOD=WATCH`, `FOLDER=/tmp/dashboards`, and no
  `REQ_URL`: it only writes the files, which the provider picks up, so it never calls Grafana and holds no
  Grafana credential.
- Token: `automountServiceAccountToken: false` on the pod; a projected volume (service account token,
  `kube-root-ca.crt`, the namespace) is mounted at `/var/run/secrets/kubernetes.io/serviceaccount` in the
  `dashboards` container only, so the `grafana` container has no Kubernetes token.
- RBAC: the Role `<release>-grafana-dashboards` (`get`, `list`, `watch` on `configmaps` in the release
  namespace, nothing else) and its RoleBinding to the ServiceAccount `<release>-grafana`.
- Security context of both containers: `runAsNonRoot`, `runAsUser` and `runAsGroup` 472 (`grafana`) and
  65534 (`dashboards`), a read-only root filesystem, no privilege escalation, `drop: [ALL]`,
  `RuntimeDefault`; `fsGroup` 472 and `supplementalGroups` [472] on the pod: the sidecar (65534) writes
  `/tmp/dashboards`, an emptyDir that `fsGroup` makes group-writable for 472 only, so the sidecar joins
  group 472 as a supplementary group (the pod's containers share it, and only grafana and the sidecar run);
  `enableServiceLinks: false`. Writable paths are emptyDirs:
  `/var/lib/grafana` (the database: lost on restart), `/tmp` of `grafana` (in memory, 16Mi: the backend plugins, the
  Prometheus datasource among them, create their sockets there) and `/tmp/dashboards` (shared by the two containers).
  Probes: `/api/health` on 3000. A change of a ConfigMap rolls the pod through a checksum annotation.

Pins and updates: the three images are stages of `images/Dockerfile` (`prometheus`, `grafana`,
`k8s-sidecar`), as Valkey and the exporter are. Dependabot's existing `docker` entry on `/images` (14-day
cooldown) proposes their bumps from the `FROM` lines; `scripts/check-pins.sh` fails until `values.yaml`
carries the same reference, so a bump PR updates both. No new Dependabot entry and no new cooldown check.

## 11. Security baseline

- Every pod mimic owns runs with `runAsNonRoot`, `readOnlyRootFilesystem`, all capabilities
  dropped, `seccompProfile: RuntimeDefault`, no service account token unless needed (the
  controller needs one from F1). The only exception is `mimic-init`: UID 0 (iptables needs
  effective capabilities, which a non-root process with `no_new_privs` cannot get), only
  `NET_ADMIN` and `NET_RAW`, `allowPrivilegeEscalation: false`, read-only root filesystem,
  `RuntimeDefault` seccomp, exits once the rules are installed.
- The controller's RBAC is minimal: `get`/`list`/`watch` on `mimicworkloads` and, from F5,
  `patch` on `mimicworkloads/status` (§7.4); `get`/`list`/`watch`/`patch`
  on its own `MutatingWebhookConfiguration` only (by name, and restricted to `caBundle` by a
  `ValidatingAdmissionPolicy`); `get`/`update` on its webhook Secret and its TLS seed Secret only (by name, in its
  namespace); `create` on `tokenreviews` (sidecar API authentication, §7.2). No access to
  pods, Deployments or other Secrets. From F11, only with `loadTest.enabled`: `get`/`list`/
  `watch` on `mimicloadtests` and `patch` on `mimicloadtests/status`; `list`/`watch` on
  `endpointslices` (pod names and addresses, §7.8); `create`/`delete`/`get`/`list`/`watch` on
  `jobs` and, with `loadTest.networkPolicy`, `create`/`delete` on `networkpolicies`, both
  restricted by a `ValidatingAdmissionPolicy` (`failurePolicy: Fail`, binding
  `validationActions: [Deny]`, `matchConditions` on the controller's service account user
  name, operations `CREATE`, `UPDATE` and `DELETE`). A `DELETE` is admitted only when the
  deleted object (`oldObject`) carries the §7.8 labels (`app.kubernetes.io/managed-by: mimic`
  and `mimic.platform.io/load-test`), so the controller deletes its own generator objects and
  no other Job or NetworkPolicy of the namespace; the namespace rule and the shape rules below
  are checked on `CREATE` and `UPDATE` only. On `CREATE` and `UPDATE` the policy admits only:
  - a namespace labelled `mimic.platform.io/inject` `enabled` or `selected` (where the
    webhook already shapes every new pod) and the §7.8 labels on the object and its pods;
  - for a Job: exactly one container and no `initContainers` or `ephemeralContainers`; image
    equal to the chart's `mimic` image, which must be given by digest when `loadTest.enabled`
    (the chart fails otherwise); command `[/usr/local/bin/mimic-load]` and no `args`; `env`
    entries with a name of §10.6 and a literal `value` only (no `valueFrom`, no `envFrom`);
    `automountServiceAccountToken: false` and service account `default`; `hostNetwork`,
    `hostPID`, `hostIPC` false or absent, no `hostPort`, no `nodeName`, `tolerations`,
    `hostAliases`, `hostUsers: true` or `shareProcessNamespace`; the §7.8 security context
    (non-root UID 65532, no privilege escalation, drop `ALL` and add none, read-only root,
    `RuntimeDefault`); volumes only one `projected` volume whose only source is a
    `serviceAccountToken` with `audience: mimic.platform.io/load` and `expirationSeconds` at
    most 600 (never the API server audience: an empty or other audience is refused), and at
    most one `secret` volume, mounted read-only at `/mimic/credentials`;
  - the Job fields the generator never sets are absent or empty: `priorityClassName`,
    `runtimeClassName`, `affinity`, `seLinuxOptions` and `appArmorProfile` (pod and
    container security context); `schedulerName` is absent or `default-scheduler` (the API
    server's default); `parallelism` equals `completions`, and `completions` is at most 4096
    (the CRD's concurrency ceiling: a pod per concurrency slot at most);
  - the `secret` volume's `secretName` equals the Job's annotation
    `mimic.platform.io/credentials-secret` (written by the controller from
    `credentials.secretName`), and the annotation is absent when the Job has no `secret`
    volume. This is a consistency check only: the policy cannot read the `MimicLoadTest`,
    and the controller writes both the volume and the annotation, so a compromised
    controller can mount any Secret of an injected namespace and the policy still admits
    the Job. Tying the Secret to the author rests on the author check of the
    `MimicLoadTest` policy (§11.1) and on the controller itself, not on this rule;
  - for a NetworkPolicy: the §7.8 shape (pod selector on the run's label, `policyTypes`
    `Ingress` and `Egress`, peers only the deployment's pods, the controller pods and the DNS
    selector).
- The webhook never logs or stores the pod spec or the patch (env values may hold secrets).
- Images are distroless, pinned by digest, scanned in CI (no fixable HIGH/CRITICAL).
- Dependencies checked by `cargo deny` (advisories, licences, sources).
- No sensitive value reaches Valkey or a log without redaction (§6). Discovery keeps no
  opaque body, no hash of a redacted value, and never logs a header value, query, body or
  sample; the local report is served on loopback only. From F5 the controller re-applies the
  redaction to every pushed report before it reaches Valkey or a log (§7.3), never logs
  report content, and serves the stored reports on loopback only (§7.5). Valkey keeps only
  redacted reports, sequence numbers and pod names.
- The UI (F6, §7.5) is read-only and renders stored, re-redacted reports only: its documents
  hold names, schemas and redacted samples, never a recorded value in a Markdown field
  (§7.6). Its pages run under a CSP that forbids inline and evaluated script and any origin but
  the listener's own; Swagger UI has no "Try it out" and no validator, so a browser on the
  UI never sends a request to a workload or to the Internet. Swagger UI is vendored at a pinned
  version, verified against its npm `sha512` and embedded in the binary.
- Egress recording (F7, §6.1) adds no privilege, Kubernetes object, RBAC rule or variable set
  by the webhook. Decrypted outbound exchanges exist only in the sidecar's memory, within the
  capture budget, and leave it only redacted, through `/discovery` on loopback; outbound
  request header values are kept only for a fixed allowlist, since the app's own provider
  credentials travel there under arbitrary names. Nothing outbound is pushed before F8. From
  F8 the redacted outbound part travels in the same authenticated push as inbound, and the
  controller applies the outbound view again (request header allowlist, `key`/`sig`, authority
  hosts and SNIs through the detectors) before anything reaches Valkey or a log (§7.3): a
  sidecar up to F7, or a faulty one, cannot store an outbound credential the controller's rules
  redact. F8 adds no privilege, RBAC rule or port.
- TLS key custody (§5): the seed exists only in its Secret and the controller's memory; root
  keys only in the controller's memory; intermediate and leaf keys only in the sidecar's
  memory (non-dumpable, no core file). Tokens and keys are never logged.
- Egress endpoint (F9, §7.7): a pod reads only its own deployment's egress set (namespace from
  its token, deployment checked against its pod name). The set is built from the re-redacted
  `:outbound` and keeps no schema, sketch or hash; it travels over the verified TLS of §7.2 and
  lives in the sidecar's memory only. `/egress` shows authorities, templates and counts to
  loopback peers, never a sample or a field name. F9 adds no privilege, RBAC rule, port,
  Kubernetes object or webhook variable; the controller's memory for it is bounded by the body
  budget (builds) and `MIMIC_SIDECAR_API_EGRESS_BUDGET` (projections), the sidecar's by
  `MIMIC_EGRESS_MAX_BYTES`.
- The sidecar API port is admitted by the controller NetworkPolicy from any source by
  default (injected pods can be in any namespace; `networkPolicy.sidecarApiFrom` narrows it).
- Load tests (F11, §7.8) are off unless the chart enables them. Load reaches only the
  addresses of the deployment's ready pods, each verified by its sidecar's `/load` probe
  before each connection, each request is refused by the target sidecar unless it is in mock
  and carries a load ticket signed by its deployment's root, and a NetworkPolicy fences the
  generator pods: the egress of a loaded pod is mocked for every request it forwards. The generator
  holds no Kubernetes API token; its load token is accepted on the load routes only, for its
  own run. Credentials come from a Secret that the `MimicLoadTest`'s author may read
  (admission `authorizer` check), mounted only in the generator pod and never read by the
  controller; plans, results, status, Valkey, logs and metrics hold field names and numbers,
  never a value. Valkey holds a run's plan and per-pod results only while it is active,
  within `MIMIC_LOAD_STORE_BUDGET` for all active runs (payload bytes; real memory can exceed it by up to 25%), and a summary of at most 192 KiB per
  ended run (20 per deployment). Quotas: `MIMIC_LOAD_MAX_RUNS` runs in the cluster,
  `MIMIC_LOAD_MAX_NAMESPACE_RUNS` per namespace, one per deployment, `MIMIC_LOAD_MAX_PENDING`
  waiting per namespace, the
  `MIMIC_LOAD_*` ceilings per run, fixed pod resources counted by the namespace's
  `ResourceQuota`, `activeDeadlineSeconds`, no pod retry; the controller's share is one plan
  build per run on the bounded render pool and one results post per pod and interval.
- The bundled Prometheus and Grafana (#379, §10.7) are off unless the chart enables them, for a
  demo or a dev cluster. Prometheus may `list` and `watch` pods only, has no admin, lifecycle,
  remote-write or OTLP endpoint, and its NetworkPolicy admits 9090 from the Grafana pods of the
  release only (none when Grafana is off) and lets it out to 15100 in its namespaces, 8081 of the
  controller pods, 9121 of the Valkey pods (with `valkey.metrics.enabled`) and the API server, and
  nowhere else (no DNS: it scrapes pod IPs). Grafana may read the ConfigMaps of the release
  namespace only, never a Secret; its admin password is in a generated Secret; anonymous access,
  plugins, analytics and update checks are off; its NetworkPolicy admits 3000 from the release
  namespace and lets it out to Prometheus on 9090, DNS (`loadTest.dnsSelector`) and the API
  server. Both Services are `ClusterIP`; no ingress is rendered. Every pod is Pod Security
  `restricted`. The existing policies need no new rule: the controller and Valkey ports already
  admit the release namespace, and the generators get the namespace peer (§10.7).

### 11.1 Accepted limits

- (#385) Whoever can reach an admin port (§9) reads the process's resource use, its configured
  limits (`mimic_limit`) and the time its discovery last found a new endpoint. They are numbers
  under fixed labels, no name or value of the traffic, and the RED counters already show as
  much activity; `mimic_proxy_overhead_seconds` gives no timing that the response time does
  not already give a client.
- While the relay holds the client's bytes behind an upgrade or `CONNECT` request, or at the
  read-ahead of 32 requests, it does not read the client socket, so a bare client FIN is not
  seen until the destination answers or closes, or the client resets: the destination holds
  the same state (it has not answered either), and the FIN could only be passed on after the
  held bytes, which must wait for the response. Watching the socket without consuming it is
  not possible on a TLS stream.
- Capture is an observability and test tool, not a security boundary: an app container with
  `NET_ADMIN`, or whose image runs as UID 1337 without declaring it in the pod spec, escapes it.
- `exec` probes are left as they are. A `tcpSocket` probe only checks that the sidecar can
  connect to the app, a `grpc` probe only supports the cleartext `grpc.health.v1` protocol.
- Rewritten probes depend on the sidecar: while it restarts, the app's liveness probe can fail
  and restart the app if the outage outlasts `failureThreshold` × `periodSeconds`. The
  `/app-health/` endpoints sit on the admin port, so the network can reach them. They only run
  the pod's own probes.
- A volatile sketch holds XXH3 hashes of values. A sidecar redacts detected forms (JWT, card,
  token prefix) before it sketches, so only a faulty or hostile one stores such a hash, and
  the controller cannot tell it from a hash of another value: it drops a sketch only when a
  retained sample shows a detected form at that field. A low-entropy value left in a sketch
  could be recovered by brute force.
- Loopback traffic (an app calling itself) and non-TCP traffic other than UDP 443 are not
  captured.
- Namespaces whose Pod Security level is `baseline` or `restricted` reject injected pods
  (`mimic-init` needs `NET_ADMIN`/`NET_RAW`); injection needs the `privileged` level. The
  rejection is visible as a `FailedCreate` event on the ReplicaSet.
- The webhook does not authenticate the API server (any client that reaches port 8443 can call
  it); it has no side effect and only computes a patch. Such a client can hold at most
  `MIMIC_WEBHOOK_MAX_CONNECTIONS` connections, each for at most `MIMIC_WEBHOOK_REQUEST_TIMEOUT`
  without completing a request, which bounds how long it can delay admission.
- The workload identity comes from the pod's `ownerReferences`, which the pod's creator
  controls: a user of the namespace can obtain a sidecar named after another deployment of the
  same namespace. The sidecar API checks only that the token's bound pod exists and has a name
  that deployment's ReplicaSets can generate (§7.2), without reading pods (an
  `ownerReference` is set by the pod's creator too, so reading it would not bind the identity
  either): such a user can push reports into another deployment of the same namespace,
  within the store bounds of §7.3, and so add endpoints, counts and samples to its discovered
  API (and, from F8, to its recordings: forged outbound endpoints and authorities, which F10
  would replay as mocks; the authority limit ranks by the counts the pods push, which such a
  user also chooses). Other namespaces are out of reach: the namespace
  comes from the token.
- Store (§7.3): a push retried more than min(24 h, `MIMIC_STORE_TTL`) after it was applied is
  applied again (its `:seq` field has expired). A pod that dies without `SIGTERM` (node loss,
  sidecar killed) loses its unshipped report: at most one push interval plus a pending
  report. A successful `TokenReview` is cached 60 s, so a deleted pod's token works up to
  60 s longer. Valkey's AOF file on its PVC is not encrypted by mimic (it holds redacted
  reports only; use an encrypted storage class). A user who can create pods in a namespace
  can fill up to `MIMIC_STORE_MAX_DEPLOYMENTS` × the per-deployment bound of Valkey memory;
  once Valkey reaches `maxmemory`, pushes of every namespace get `503` until data expires or
  memory is added. The per-source rate limit assumes pod source addresses are not translated
  between nodes (a CNI that masquerades pod traffic makes the pods of a node share a bucket).
  `status.discovery.pods` can differ by one between replicas at a window edge.
- The `caBundle` of the webhook configuration and the data of the webhook Secret are owned by
  the controller: GitOps tools must ignore those fields.
- Only the `iptables-nft` backend is supported (the node kernel needs `nf_tables`).
- `mimic-init` flushes the built-in chains it hooks in the pod's network namespace: another
  tool that installs iptables rules in the same pod (another mesh) is not supported.
- TLS (§5): the first connection to a destination whose client rejects the mimic certificate
  (pinning, compiled-in roots such as `webpki-roots`, its own truststore) fails; later ones
  pass through. Remembered destinations are lost when the sidecar restarts, and at most 4096
  are kept.
- A container whose spec already defines one of the trust variables keeps it (it trusts what
  it chose; its connections fall back as above). Variables set in the *image* (`ENV`) cannot
  be seen by the webhook: an image-level `JAVA_TOOL_OPTIONS` is masked by mimic's (defining
  it in the pod spec restores it, mimic then prepends); image-level `SSL_CERT_FILE`,
  `REQUESTS_CA_BUNDLE`, `CURL_CA_BUNDLE` and `NODE_EXTRA_CA_CERTS` files are merged by
  `mimic-trust`. JVM certificates added only to the JDK's own `cacerts` (not to the system
  store) are not in the mimic truststore. The truststore password `mimic` is public: it
  protects nothing secret.
- The sidecar's upstream roots are the union of the stores of all the pod's images: a
  container trusts, through interception, a CA that only another image of the pod has.
  `webpki` is stricter than OpenSSL (e.g. certificates without SAN): such destinations are
  passed through (`upstream_untrusted`), and only visibility is lost.
- Leaves name only the SNI (or the destination IP); a client that verifies another name is
  handled by the fallback. TLS inside an HTTP `CONNECT` tunnel, STARTTLS and Encrypted Client
  Hello are not intercepted (ECH falls back as `client_rejected`).
- A user who can create pods in a namespace can obtain an intermediate for another deployment
  of the same namespace (pod named after it, as for the workload identity above), and so
  impersonate any host to that deployment's pods if they also sit on their network path. A
  compromised controller (or anyone reading the seed Secret) can impersonate any host to every
  injected pod.
- Discovery redaction (§6) is a fixed list of names and detectors plus
  `spec.discovery.redact`: a secret under an unlisted name and in a form no detector
  recognises (e.g. a random API key in a custom header) is kept in samples, served by
  `/discovery` and, from F5, stored in Valkey. Personal data that is not a credential (names,
  e-mail addresses, amounts) is kept in samples by design; the `@` rule only keeps it out of
  path templates. Inbound HTTPS is not discovered.
- UI (F6): anyone allowed `pods/portforward` on the controller pods reads the discovered API
  of every namespace (the port-forward carries no user identity, so there is no
  per-namespace authorisation); grant it like read access to all injected namespaces. Any
  process of the workstation that runs `kubectl port-forward` can reach the forwarded port,
  as with every port-forward. The UI shows what redaction kept (the limits above apply to
  it). Response schemas are per endpoint and media type, not per status (§6): a `200` and a
  `404` with the same media type show the same merged schema; a status whose samples were
  all refused by the sample budget shows a description only. Inferred documents describe
  observed traffic, not a contract: optional fields never sent, and statuses never seen, are
  missing.
- Egress recording (F7, §6.1): only cleartext and intercepted HTTP is recorded. Passed-through
  TLS (pinning clients, mTLS destinations, exclusions, no CA, ECH), `CONNECT` tunnels and
  non-HTTP TCP are only counted per authority (`tunnels`). Recorded latency is per request:
  DNS, TCP connect and the TLS handshake are not in it, so a mock replaying it (F10) is
  faster than the provider on new connections. There is no host templating: an app calling
  one host per tenant or bucket fills `MIMIC_DISCOVERY_MAX_AUTHORITIES`, later authorities are
  only counted (`authority_limit`). Outbound `404`/`405` create endpoints, so an app that
  forwards paths chosen by its clients records them, within the endpoint limit and template
  absorption. The outbound header allowlist does not cover queries and bodies: a credential
  there under an unlisted name, in a form no detector knows, is kept, as in §6. Until F8 the
  outbound part lives in the sidecar's memory only: a restart loses it, and anyone allowed
  `pods/portforward` on the app's pods reads its redacted provider exchanges (response bodies
  included, personal data in them too) through `/discovery`. A spoofed `Host` sent by the app
  (or forwarded by it from its clients) can fill `MIMIC_DISCOVERY_MAX_AUTHORITIES`; later
  authorities are only counted.
- Egress aggregation (F8, §6.1, §7.3): the redacted provider exchanges of every injected
  deployment (response bodies and the personal data in them included) are stored in Valkey and
  shown by the UI: anyone allowed `pods/portforward` on the controller pods reads them for every
  namespace, as for inbound. While a sidecar falls back to inbound only (§7.2), its outbound
  part waits in its memory within the §6.1 bounds and is lost if the pod stops before a
  controller accepts it; a controller rolled back below F8 keeps serving inbound data and leaves
  `:outbound` to expire, so outbound data recorded meanwhile is lost. The store has no fixed
  split of the sample budget between directions: a direction may hold more than half while the
  other uses less, and gives the room back only as the other direction's new status classes
  arrive. The status `Endpoints` column of a `MimicWorkload` reads `0` between a chart upgrade
  (new CRD) and the first status apply of an F8 controller. Each deployment's store bound grows
  by its outbound endpoints and authorities (`MIMIC_STORE_MAX_ENDPOINTS` and
  `MIMIC_STORE_MAX_AUTHORITIES`; the sample budget is shared), which Valkey sizing must allow.
  An inbound-only fallback after an attempt with an unknown outcome drops that sequence's
  outbound part (§7.2) rather than risk counting it twice: at most one push of outbound data per
  such event, counted and logged.
- Egress endpoint (F9, §7.7): every pod of a deployment holds the redacted outbound samples
  recorded by all its replicas (provider responses and the personal data in them included), so
  whoever controls one pod of the deployment reads what its other replicas exchanged with
  providers, within redaction. A user who can create pods in a namespace can pull the egress
  set of any deployment of that namespace (a pod named after it, as for the workload identity
  above); creating pods already gives more (the namespace's Secrets can be mounted). In
  discovery mode the outbound part changes with almost every push, so a busy deployment's
  pulls are mostly `200` and builds (at most one per digest per replica): the cost grows with
  pods ÷ `MIMIC_EGRESS_PULL_INTERVAL` × the size of the set. A set larger than
  `MIMIC_SIDECAR_API_MAX_EGRESS` (default 4Mi, sized for the sidecar's memory limit) is not
  served (`507`), nor is an `:outbound` too large for the body budget: that deployment's pods
  keep an older snapshot, or none. With the default store sample budget (4Mi for both
  directions), a deployment whose outbound samples use most of it can exceed 4Mi. A restarted sidecar has no snapshot until its next pull, at most one
  interval later.
- Value detectors match a whole value (or a whole header credential): a Google `AIza` key, a
  token prefix or a JWT embedded inside longer free text (`note AIza…`) is not caught by value;
  only by name, when its field or header name is a redacted name.
- Discovery is best effort under load: dropped records, a full sample budget or the endpoint
  limit make counters and samples partial (counted in §9 metrics), never slow traffic. A
  `404`/`405` reaching a path no other status has created is not recorded. Template
  absorption can merge distinct resources below one node (an API with more than
  `MIMIC_DISCOVERY_MAX_LITERALS` literal routes at one level, e.g. static files at the root).
- Emptying the seed Secret rotates every root: running sidecars then fail the chain check
  and pass everything through (`no_ca`) until their pod is recreated. The trust wiring and
  `spec.tls.exclude` apply at pod creation.

- (F10) **Mock is HTTP only and does not answer DNS.** The app still resolves the names it
  calls (UDP DNS is not captured, §4): in an environment that cannot resolve a provider's name
  the app never connects, and the mock is not reached. Non-HTTP cleartext TCP is relayed to the
  real destination in mock (§8), and so is every `spec.mock.passthrough` destination; a client
  that stays silent for 1 s on a new connection makes a TCP handshake (no byte) reach its
  destination, and server-first protocols wait 1 s more per new connection in mock.
- (F10) **Mode changes are eventually consistent.** A change reaches a pod within one pull
  interval (60 s by default) while the controller answers; a sidecar that cannot reach it keeps
  the mode it has (stuck in mock, or in discovery), which `status.mode.otherPods` shows. A
  restarted sidecar container resumes the mode it saved in the pod (§8 "Mode state"), so a
  restart does not reopen real egress; only a pod created in the instant before a change starts
  in the old mode, for the round trip of its first pull (sent at once). `spec.mock.passthrough`
  applies to pods created afterwards, like `spec.tls.exclude`.
- (F10) **Roll back only out of mock.** A rollback of mimic to F9 keeps running F10 sidecars in
  their mode, but every pod recreated afterwards gets an F9 sidecar, which has no mock and
  forwards for real; `spec.mock` is pruned by the old CRD schema (§7).
- (F10) **Whoever may create or edit a `MimicWorkload` in a namespace decides the egress of
  every deployment there** (mock wins, §7): Kubernetes RBAC on `mimicworkloads` is the
  authorisation, as it already is for injection and `spec.tls.exclude`.
- (F10) **Replay fidelity.** A mock replays recorded samples, chosen per request: no state
  across calls (a created resource is not readable afterwards), no sequence. Opaque bodies
  (binary, protobuf, over 256 KiB) are replayed empty; gRPC always answers `OK` with an empty
  message; redacted values are replayed as `<redacted>` (an OAuth token endpoint returns that
  as the token, which later mocked calls accept, since header values are not compared). A
  deployment's mock answers are built from what its own pods recorded, values that no detector
  catches (names, e-mail addresses) included, and are sent to its own app only; a pod of the
  deployment that pushes crafted reports can shape what its sibling pods are answered (the same
  trust domain as discovery, §7.2).
- (F10) In mock the sidecar issues leaves for any name a client of the pod asks for, without a
  verified upstream certificate: they chain to the deployment's own root, which only this
  deployment's pods trust, and no traffic leaves, so nobody outside the pod relies on them.
- (F10) The mock index compares the fields of a request with those of a sample by unkeyed
  64-bit hashes (XXH3) of their names and values, to bound its memory. The app that sends the
  request can craft a collision and so make a request count as `exact` with a sample of the
  same endpoint it would otherwise have missed by one field. It chooses among answers of that
  endpoint only, and the app is the pod's own client: nothing is gained that the app could not
  get by sending the sample's request itself.

- (F11) **In-flight requests at a switch out of mock.** A load request forwarded while its
  pod was in mock can make outbound calls after the pod switched to discovery (the app's own
  processing time): at most the run's concurrency of requests can reach real destinations
  then. Leave mock after the run ended.
- (F11) **The mocked deployment still uses its real datastores** (non-HTTP TCP is relayed,
  §8) and its `spec.mock.passthrough` destinations: the load reaches them through the app.
  A shared database or an internal service kept real is loaded too.
- (F11) **A load ticket is a bearer value until its run's deadline.** Whoever holds it (the
  generator pods, and the load API answer to them) can keep requests to the deployment's pods
  out of discovery while they are in mock; anyone can get `599` for their own requests by
  adding the header. Capture is not a security boundary (above).
- (F11) **Load requests are only as redacted as discovery.** Requests replay the stored
  samples, so a secret value that no detector catches (§6) is sent to the deployment's pods
  and held in the plan (in Valkey while the run is active), as it already is in the stored
  report. Names to redact are added with `spec.discovery.redact` (§7); the plan holds what the
  report holds, nothing more.
- (F11) **Fair use is per namespace.** `MIMIC_LOAD_MAX_NAMESPACE_RUNS` (2) out of
  `MIMIC_LOAD_MAX_RUNS` (4) lets two namespaces hold every slot; an operator lowers the first
  or caps the objects with a `ResourceQuota` on `count/mimicloadtests.mimic.platform.io`.
- (F11) **The generator NetworkPolicy needs a CNI that enforces it**, and it admits any pod
  labelled `mimic.platform.io/deployment: <deployment>` in the namespace (a namespace user can
  set that label): the probe and the ticket are the guarantee, the policy a second fence.
- (F11) **The namespace is the trust domain of a run.** A user of the namespace who can create
  a pod named like the deployment's pods behind a Service makes it a target and receives the
  load and its credentials (that user could mount the Secret anyway); one who can create a pod
  named like the run's Job pods can post forged results for that run.
- (F11) **Load and credentials travel in cleartext** between the generator and the target
  pods, as the recorded traffic did (inbound TLS is not discovered, §5); a NetworkPolicy or an
  encrypting CNI protects them.
- (F11) **Results are client-side**: latency includes the network and the generator; pods of
  one run start when Kubernetes schedules them, so their phases can be shifted by seconds;
  percentiles are bucket bounds (at most 4.43 % high).
- (F11) A run without targets behind a Service cannot be load-tested (targets come from
  `EndpointSlice`s, so that the controller reads neither pods nor Deployments).
- (#378) The chart adds no NetworkPolicy on the sidecar's admin port (15100) in app namespaces:
  they belong to their owners, and a policy that selects app pods with `policyTypes: [Ingress]`
  would isolate them. The port is reachable from any pod that reaches the pod IP, as before; the
  namespace owner adds the policy.
- (#378) The exporter's NetworkPolicy (9121 from the release namespace and
  `networkPolicy.metricsNamespaceSelector`) needs a CNI that enforces NetworkPolicy, as the other
  policies of the chart do (FND-08). kind's default CNI does not.
- (#379) **The bundled Prometheus reads every pod object of its namespaces**, literal `env`
  values included (a secret set as a plain env value, not a `secretKeyRef`, is readable by it).
  Service discovery asks with label selectors, but RBAC cannot filter by label.
  `monitoring.prometheus.namespaces` narrows it to a list of namespaces.
- (#379) **The Prometheus HTTP API has no authentication.** Whoever reaches 9090 reads every
  series: authorities of the egress, endpoint templates of load tests, pod and deployment names.
  The NetworkPolicy (Grafana pods only) needs a CNI that enforces it, and `kubectl port-forward`
  (RBAC `pods/portforward` in the release namespace) bypasses it by design: it is how the demo is
  reached.
- (#379) **Grafana has one admin user** and no SSO. Its password is in the Secret
  `<release>-grafana-admin`, mounted as a file in the `grafana` container (whoever reads that
  Secret or execs into that container has it). A render without a cluster (`helm template`,
  GitOps tools) cannot `lookup` and draws a new password at each render, which rotates it at each
  sync and restarts Grafana, as for Valkey.
  The Grafana database is an emptyDir: users, preferences and edited dashboards are lost on restart.
- (#379) The dashboard sidecar can read every ConfigMap of the release namespace, not only the
  labelled ones: RBAC cannot filter by label. The chart puts no secret in a ConfigMap, and the
  token is mounted in the sidecar container only.
- (#379) The bundled Prometheus keeps 24 hours on an emptyDir and runs one replica: data are lost
  on restart, there is no high availability. Its image (`quay.io/prometheus/prometheus`) is built
  on busybox, not distroless (upstream ships no distroless image), so it holds a shell.
- (#379) The egress to the API server (TCP 443 and 6443) admits any destination on those ports
  unless `networkPolicy.apiServerCIDR` is set, since its address depends on the cluster.
- (#379) The NetworkPolicies of Prometheus and Grafana are checked as rendered objects only (MON-20):
  kind's default CNI does not enforce them, so their reachability is not tested end to end (follow-up:
  an enforcing CNI in the e2e).
- (#379) The Prometheus and Grafana templates are the chart's own: their upgrades (new flags,
  configuration changes of a major version) are carried by hand with the image bump, which
  Dependabot proposes and MON-21 tests.

## 12. Delivery plan

One GitHub issue and one PR (or a short stack) per feature. Acceptance criteria are written in
this section by the architect at the start of each feature, then implemented test-first; test
names contain the criterion ID (`fnd_02_…`).

| Feature | Content | User plan point |
|---|---|---|
| F0 | Foundations: workspace, runtime plumbing, image, Helm chart with Valkey, kind e2e, CI/CD, agent team | — |
| F1 | Injection: webhook, native sidecar, `mimic-init`, selection by label/CRD, opaque TCP tunnel | 1 |
| F2 | HTTP-aware pass-through proxy, access logs, RED metrics, inbound and outbound | 2 |
| F3 | TLS interception, per-deployment CA, trust injection, fallback | — |
| F4 | Ingress endpoint discovery: templating, schemas, redaction | 3 |
| F5 | Controller collects endpoints into Valkey, merges per deployment | 4, 5 |
| F6 | UI: OpenAPI 3.1 + Swagger UI per deployment | 6 |
| F7 | Egress recording in the sidecar: samples, latency histograms | 7 |
| F8 | Egress shared with the controller, aggregated per deployment | 8, 9 |
| F9 | Egress endpoint for sidecars | 10 |
| F10 | Mock mode, and back to discovery | 11, 12 |
| F11 | Load test of a mocked deployment | (added 2026-10-31) |

### 12.1 F0 — Foundations

| ID | Criterion |
|---|---|
| FND-01 | A `MIMIC_*` variable with an invalid value stops the process before it listens, with exit code 2 and a stderr message that names the variable and the value. A missing required variable does the same. |
| FND-02 | `/healthz` answers 200 as long as the process runs. `/readyz` answers 200 when the component is ready, 503 otherwise. |
| FND-03 | `/metrics` answers 200 in OpenMetrics text format (scraped natively by Prometheus) and contains `mimic_build_info{component="<component>",version="<crate version>"} 1`. |
| FND-04 | Every line written on stdout is one JSON object with `timestamp`, `level`, `target` and `message`. `MIMIC_LOG` filters it. |
| FND-05 | On `SIGTERM` the process stops accepting connections, lets in-flight requests finish, and exits 0 within `MIMIC_SHUTDOWN_GRACE`; it logs the start and the end of the shutdown. |
| FND-06 | The controller is ready only when Valkey answers `PING` (with the configured password); it becomes unready, without exiting, when Valkey is unreachable, and ready again when it comes back. |
| FND-07 | The image runs as UID 65532, contains no shell, and both binaries answer `--version` with the crate version. |
| FND-08 | `helm lint` passes; every rendered pod is Pod Security `restricted` (non-root, read-only root filesystem, no capability, `RuntimeDefault` seccomp, no privilege escalation). Valkey requires a password generated at install and kept on upgrade, and only accepts connections from the controller (NetworkPolicy; needs a CNI that enforces NetworkPolicy, as kind's default does). |
| FND-09 | On kind (Kubernetes ≥ 1.33), `helm install` brings the controller and Valkey to Ready, and the controller's `/readyz` and `/metrics` answer through a port-forward. |
| FND-10 | CI runs as dependent stages: lint → unit and integration tests → image build and scan → kind e2e, plus a chart stage (`helm lint --strict`; the release-pinned chart renders both mimic images by digest, `scripts/check-release-chart.sh`; the pins of `images/Dockerfile` match `values.yaml` and the kind node image in ci.yml, `scripts/check-pins.sh`) and a supply-chain stage: `cargo deny` (advisories, licences, sources, and the bans of `deny.toml`, which deny the Brotli crates), `zizmor`, `actions/dependency-review-action` on a pull request, a dependency cooldown (`scripts/check-cooldown.py`: a crate that a pull request adds or bumps in a `Cargo.lock` must be published at least 14 days before, unless `scripts/cooldown-allowlist.toml` lists it with a reason), gitleaks on a pull request, `trivy config` on the rendered chart and on the Dockerfile (HIGH and CRITICAL fail), and CodeQL for Rust and GitHub Actions. The image job scans the mimic, mimic-init and Valkey images with Trivy; an accepted finding is listed in `.trivyignore.yaml` with its reason, its issue and an expiry date, after which the scan fails again. A weekly and a push-to-main OpenSSF Scorecard run reports its result without gating. A single `ci-ok` check sums up every job except Scorecard: it fails if any job fails or is cancelled, and accepts a skipped job only when the job's own path filter excluded it (the HTTP/1 codec fuzz job, PRX-28, on a change outside `crates/mimic-core/src/proxy/http1/`, `crates/mimic-core/src/proxy.rs`, the workspace `Cargo.toml` and `Cargo.lock`, and `fuzz/`; the discovery fuzz job (one matrix job per target: the recorder, the egress set and the mock; DSC-23, PUL-20 and MCK-20), on a change outside `crates/mimic-core/src/discovery/`, `crates/mimic-core/src/mock/`, `crates/mimic-core/src/mock.rs` and `fuzz/`). A `v*` tag is released only from a commit on `main`; it publishes multi-arch signed images with SBOM and provenance, and the signed Helm chart pinned to the digests of both mimic images. From F6 the cooldown also covers the npm lockfile of the vendored Swagger UI, and the supply-chain stage checks the vendored files against it (UI-13). |

### 12.2 F1 — Injection

Levels: **U** unit in `mimic-core`, **I** integration in `crates/*/tests`, **E** kind e2e in
`scripts/e2e.sh`, **C** CI pipeline.

| ID | Criterion |
|---|---|
| INJ-01 | **U.** The selection decision follows the table of §7.1 for every combination of namespace label (`enabled`, `selected`, absent), pod label (`enabled`, `disabled`, absent) and presence of a `MimicWorkload` whose `spec.deployment` is the pod's deployment; the excluded namespaces are never injected. The deployment name is derived from the controlling ReplicaSet owner and the `pod-template-hash` label; any other owner gives `unsupported-owner`. |
| INJ-02 | **U.** A pod with `hostNetwork: true`, a container port 15001, 15006 or 15100, or `runAsUser: 1337` (pod or app container) is skipped with the matching reason; the patch only adds `mimic.platform.io/skipped: <reason>`. A pod that already has a `mimic-sidecar` init container gets an empty patch. |
| INJ-03 | **U.** The injection patch is a JSON Patch, byte-identical for the same input, that puts `mimic-init` then `mimic-sidecar` before the pod's own init containers (also when it has none), rewrites the app containers' `httpGet`, `tcpSocket` and `grpc` probes as in §4 and changes nothing else in them, and adds `mimic.platform.io/injected: <version>`. `mimic-init`: UID 0, `allowPrivilegeEscalation: false`, drop `ALL`, add only `NET_ADMIN`/`NET_RAW`, read-only root, `RuntimeDefault`. `mimic-sidecar`: `restartPolicy: Always`, UID/GID 1337, `runAsNonRoot`, no capability, read-only root, `RuntimeDefault`, startup probe `/readyz` and liveness probe `/healthz` on 15100, the configured resources and the §10.3 variables. Neither has a volume mount (amended by TLS-11). From F11 the patch also adds the pod label of LDT-20 (`mimic.platform.io/deployment: <deployment>`, created when `metadata.labels` is absent, replacing a value the pod set), unless the deployment name is longer than 63 characters: that is not a valid label value, so such pods get no label and are no load test target (§7.8); a `mimic.platform.io/deployment` label the pod already carries is removed by a `remove` operation (only when present, since `remove` of an absent path fails the patch). |
| INJ-04 | **U.** `MIMIC_EXCLUDE_INBOUND_PORTS` given to `mimic-init` holds only 15100: probe ports are not excluded. `MIMIC_APP_PROBES` holds every original `httpGet`, `tcpSocket` and `grpc` probe under its `<container>/<kind>` key, with the named port resolved and the probe's `timeoutSeconds` (`tcpSocket`: `{"tcpSocket":{"port","host"?},…}`, `grpc`: `{"grpc":{"port","service"?},…}`, `httpGet`: the `HTTPGetAction` fields); a probe whose named port does not resolve is left unchanged and absent from `MIMIC_APP_PROBES` (it fails as it would without mimic). Each rewritten probe points to `/app-health/<container>/<kind>` on 15100 with the original timings and thresholds. |
| INJ-05 | **U.** The `iptables-restore` input generated for IPv4 and for IPv6 is deterministic and: redirects inbound TCP to a local destination (`addrtype LOCAL`) to the inbound port except the excluded ports; returns outbound traffic of the proxy UID and on `lo`; redirects other outbound TCP to the outbound port; rejects outbound UDP to port 443 in `filter`; uses only mimic-owned chains (`MIMIC_INBOUND`, `MIMIC_OUTPUT`) plus one jump from each built-in chain it hooks, which it flushes first (§4). |
| INJ-06 | **I.** `mimic-init` pipes the rules to `iptables-restore` and `ip6tables-restore` found on `PATH`, logs one JSON line `redirection installed` with the families and excluded ports, and exits 0; it skips IPv6 with an INFO log when the pod has no IPv6; it exits 1 with an ERROR log naming the family when a restore command fails or is missing, and 2 on an invalid variable (FND-01). It never logs other content of its environment. |
| INJ-07 | **I.** The sidecar binds its inbound and outbound listeners from §10.3 before `/readyz` answers 200; the outbound listeners accept connections on loopback only. A connection without an original destination, or whose original destination is a sidecar listener, is closed and counted in `mimic_tcp_connections_total{direction,result}` (`no_original_dst` or `loop`, PRX-10); tunnelled connections are counted with `result="ok"` or `result="connect_error"`. |
| INJ-08 | **I.** The webhook answers only HTTPS on `MIMIC_WEBHOOK_ADDR`. `POST /inject/namespace`, `/inject/pod` and `/inject/workload` with an `AdmissionReview` v1 return an `AdmissionReview` with the same `uid`, `allowed: true` and a base64 `JSONPatch` (or none when the patch is empty). A malformed body gets 400, a body over 4 MiB gets 413, another method gets 405, and an internal error gets `allowed: false` with a message (fail-closed). |
| INJ-09 | **I.** Each admission writes one log line (`target` = `webhook`) with namespace, pod name or `generateName`, deployment, `result` (`injected`, `skipped`, `error`) and `reason`, and increments `mimic_webhook_admissions_total{result,reason}`. Neither the logs nor the metrics contain any env value, annotation value or other pod spec content (checked with a canary env value). |
| INJ-10 | **U.** The certificate decision is a pure function of the stored certificates and the current time (passed in): no or unparsable CA → new CA and serving certificate; serving certificate missing, expiring within 30 days or with the wrong SAN → new serving certificate signed by the existing CA; otherwise keep. |
| INJ-11 | **E.** After install, the webhook Secret holds a CA and a serving certificate for `<service>.<namespace>.svc`, the `caBundle` equals the CA, and `mimic_webhook_certificate_expiry_timestamp_seconds` is exposed. With 2 controller replicas both serve the same certificate; after a restart and a `helm upgrade` the certificate and `caBundle` are unchanged. The controller is unready until its CA is in every webhook's `caBundle`, the certificate is loaded (in that order) and the `MimicWorkload` cache is synced. |
| INJ-12 | **E.** The chart installs the `MimicWorkload` CRD (an invalid `spec.mode` or a missing or invalid `spec.deployment` is rejected, a change of `spec.deployment` is rejected, `spec.mode` defaults to `discovery`), a `MutatingWebhookConfiguration` with the three webhooks of §7.1 (`failurePolicy: Fail`, `sideEffects: None`, `reinvocationPolicy: IfNeeded`, pod `CREATE` only, excluded namespaces), and the RBAC of §11: `kubectl auth can-i` as the controller service account is `yes` exactly for the listed verbs and `no` for creating Secrets, reading other Secrets, reading pods and patching another webhook configuration. Patching anything but `caBundle` of its own configuration as that account is denied by the `ValidatingAdmissionPolicy`. |
| INJ-13 | **E.** In a namespace labelled `enabled` (Pod Security `privileged`), a Deployment with an HTTP readiness and liveness probe on its API port, a `tcpSocket` probe on a second port (rewritten to `/app-health/…` and its port still intercepted: a connection to it from another pod increments the inbound counter) and an init container that opens a TCP connection to a Service rolls out Ready: `mimic-init` terminated with exit code 0, `mimic-sidecar` running, the injected annotation present, no restart. A request from another pod to the app Service succeeds and increments the sidecar's `mimic_tcp_connections_total{direction="inbound",result="ok"}`; a request from the app container to another Service succeeds and increments `{direction="outbound",result="ok"}`. The pod's readiness probe points to `/app-health/…` on 15100. When the app's readiness path starts failing, the pod becomes NotReady, and Ready again when the path recovers. |
| INJ-18 | **I.** The sidecar serves `GET /app-health/<container>/<kind>` on its admin port only for the probes in `MIMIC_APP_PROBES`, and 404 for any other path under `/app-health/`. For a `tcpSocket` probe it answers 200 when it connects to the port and 503 when the connection is refused or exceeds `timeoutSeconds`; for a `grpc` probe it answers 200 only when `grpc.health.v1.Health/Check` (h2c, the probe's `service`) answers `SERVING`, and 503 for `NOT_SERVING`, `UNKNOWN`, a gRPC error, a non-gRPC server, a refused connection or a timeout. For an `httpGet` probe: It forwards the original method, path, port and headers and answers the app's status code (2xx, 3xx without following, 4xx and 5xx unchanged). It answers 503 when the app refuses the connection or exceeds the probe's `timeoutSeconds`, and talks HTTPS without verifying the certificate when the probe's scheme is HTTPS. An invalid `MIMIC_APP_PROBES` exits with code 2 (FND-01). |
| INJ-14 | **E.** From an injected app container, a UDP datagram to port 443 is refused (connected socket reports `ECONNREFUSED`), while DNS resolution over UDP 53 works. |
| INJ-15 | **E.** On the cluster: pods of an unlabelled namespace are unchanged; in a `selected` namespace only the deployment with a `MimicWorkload` is injected; a pod labelled `disabled` in an `enabled` namespace is unchanged; a pod labelled `enabled` in an unlabelled namespace is injected; the controller's namespace is never injected; labelling a namespace does not restart its running pods. |
| INJ-16 | **E.** With the controller scaled to 0, creating a pod in an `enabled` namespace fails (`FailedCreate` naming the webhook) while pods of unlabelled namespaces are created; once it is back, the ReplicaSet creates injected pods. In an `enabled` namespace enforcing Pod Security `baseline`, the injected pod is rejected by Pod Security (§11.1). |
| INJ-19 | **I.** With `MIMIC_WEBHOOK_MAX_CONNECTIONS=4` and `MIMIC_WEBHOOK_REQUEST_TIMEOUT=500ms`: a TCP client that never starts TLS, and a TLS client that sends headers byte by byte without finishing, are each closed within the timeout (+ 1 s margin); while 4 such clients are connected, a well-formed AdmissionReview is answered once they are closed, within 2 × the timeout; an idle keep-alive connection is closed after 60 s (test with an injectable idle duration). Invalid values of both variables exit with code 2 (FND-01). |
| INJ-20 | **I.** The controller watches its `MutatingWebhookConfiguration`: when a webhook's `caBundle` is changed or emptied by another client, the controller restores the Secret's CA in every webhook within 10 s, without issuing a new certificate (process test against the fake API server). |
| INJ-21 | **U.** Every literal `env` value in the patch has each `$` doubled (`MIMIC_APP_PROBES` with a probe path `/x/$(HOME)/$$` and a header value `a$b` decodes, after applying the kubelet's expansion rules, to the original probes). A pod with an app container named `mimic-init` or `mimic-sidecar`, or with an init container named `mimic-init` and no `mimic-sidecar`, is skipped as `name-conflict`. |
| INJ-22 | **I.** Running `mimic-init` twice, and after a first run whose IPv6 restore failed, results in rules that contain exactly one jump from each built-in chain per family (the fake restore scripts record the inputs; the generated input makes the jumps idempotent). **U.** With only `::1` (or no line) in `/proc/net/if_inet6` content, IPv6 is skipped; with a link-local or global address it is installed (unit test of the parser). |
| INJ-23 | **E.** By default the chart runs 2 controller replicas with a PodDisruptionBudget `maxUnavailable: 1` and preferred anti-affinity on `kubernetes.io/hostname`; the controller NetworkPolicy exists and the webhook still admits pods with it applied; `helm uninstall` leaves the CRD and an existing `MimicWorkload`, and a reinstall with the same release name succeeds and keeps it. |
| INJ-17 | **C.** `ghcr.io/thaliph/mimic-init` is built multi-arch, contains no shell, passes the image scan (no fixable HIGH/CRITICAL), and is signed with SBOM and provenance on release. The kind e2e runs on Kubernetes 1.33 (floor) and on the latest kind node image. |

### 12.3 F2 — HTTP-aware proxy, access logs, RED metrics

Levels as in §12.2. Integration tests (**I**) run the real sidecar binary or its listeners
in-process with an injected original destination, as the F1 tunnel tests do.

| ID | Criterion |
|---|---|
| PRX-01 | **U.** Detection (§4.1) is a pure function of the bytes read so far that returns `need_more`, `tls` (with the SNI when the ClientHello has one), `http1`, `h2` or `opaque`. A ClientHello split over several reads or TLS records gives `need_more` until its SNI can be read; HTTP/1.0 and 1.1 request lines with standard and extension methods give `http1`; the 24-byte HTTP/2 preface gives `h2`, a prefix of it `need_more`; a first byte that can start none of them gives `opaque` at once; 16 KiB without a decision gives `opaque` (or `tls` without SNI for a TLS record). It never panics and never reads past the buffer for arbitrary input (property test). |
| PRX-02 | **U.** Authority normalisation lower-cases, removes a trailing dot and the default port (80 for HTTP), keeps bracketed IPv6 literals, and maps anything that is not a host name or IP literal (or longer than 253 bytes) to `(invalid)`. The status class of 100–599 is `1xx`…`5xx`. The authority budget keeps the first N distinct values and returns `(other)` afterwards, the same for the same sequence of inputs (amended by PRX-23). |
| PRX-03 | **I.** HTTP/1.1 through the inbound and the outbound listener: the destination receives the request line (query included), header names with their case, header order, values, body and framing (`content-length` and `chunked` with trailers) as sent by the client, and the client receives the status, reason phrase, headers and body as sent by the destination. No header is added (no `date`, `server`, `via`, `x-forwarded-*`). Several requests on one keep-alive connection, pipelined requests included, use one destination connection. A request whose `Host` names another server is still sent to the original destination. |
| PRX-04 | **I.** HTTP/2 prior knowledge: concurrent streams on one client connection reach the destination on one connection with the same pseudo-headers, headers and bodies; a unary gRPC call returns its message and its `grpc-status`/`grpc-message` trailers unchanged. No header is added. |
| PRX-05 | **I.** A destination that speaks first (banner) and a client that sends non-HTTP bytes are tunnelled byte-for-byte in both directions. A TLS client completes its handshake with a TLS destination through the sidecar and sees the destination's own certificate (inbound, or outbound without `MIMIC_TLS_CA`; amended by TLS-17). |
| PRX-06 | **I.** A WebSocket upgrade answered `101` and a `CONNECT` answered `200` carry bytes in both directions after the response; each produces one access log line with `upgrade: true`. |
| PRX-07 | **I.** No synthesised response: when the destination refuses the connection, the client connection is closed without any byte; when it closes (FIN) or resets after receiving an HTTP/1 request, the client sees a FIN or a reset respectively, with no response byte (`result: upstream_reset`, `status_class="none"`); on HTTP/2 the stream is reset and the other streams of the connection complete. The sidecar never answers `400` or `431` on HTTP/1: a malformed or oversized head is forwarded unchanged (PRX-29). |
| PRX-08 | **I.** Each HTTP exchange writes exactly one access log line and each TLS or opaque connection one line, with the fields and values of the §4.1 table for a known exchange (direction, protocol, dst, authority, sni, method, path without query, status, grpc_status, result, durations, byte counts). A canary value placed in a header, a cookie, the query string and the request and response bodies appears in no log line. A path with quotes, backslashes or percent-encoded CR/LF stays inside one valid JSON line; a path over 1024 bytes is truncated. |
| PRX-09 | **I.** After a known set of exchanges, `/metrics` shows the exact values of `mimic_http_requests_total`, the `_count` of `mimic_http_request_duration_seconds`, `mimic_proxy_connections_total` (TLS counted under its SNI) and `mimic_proxy_bytes_total`, with the §9 labels. With `MIMIC_METRICS_MAX_AUTHORITIES=2`, a third authority is counted under `authority="(other)"`. An invalid value (`0`, `abc`, `10001`) exits with code 2 (FND-01). |
| PRX-10 | **I.** A connection without an original destination is closed and counted with `result="no_original_dst"`; one whose original destination is a sidecar listener with `result="loop"` (this splits the INJ-07 `loop` count). |
| PRX-11 | **I.** No idle timeout: an opaque tunnel and an HTTP/1 keep-alive connection left idle for 35 s (longer than `hyper`'s default 30 s header-read timeout) still carry the next exchange. A client half-close reaches the destination, which can still answer. When the destination closes an idle keep-alive connection, the client sees the close within 1 s. The client-side and destination-side sockets have TCP keepalive set to 300 s / 30 s / 4 probes. |
| PRX-12 | **I.** Forward, never refuse: 1000 simultaneous connections are all accepted and served. The sidecar's open-file soft limit equals its hard limit (`/proc/<pid>/limits`), or the PRX-25 target when the hard limit is unlimited. While its stdout is not read, requests keep completing; once stdout is read again, `mimic_log_lines_dropped_total` is above zero and the dropped lines are absent, not truncated. |
| PRX-13 | **I.** On `SIGTERM`, a response being streamed completes, new connections are refused, an idle HTTP/1 connection is closed, an HTTP/1 connection with a pending request is closed after its response with no byte added (no `connection: close`), an HTTP/2 client receives `GOAWAY`, and the process exits 0 within `MIMIC_SHUTDOWN_GRACE` (FND-05). Connections still open at the end of the grace are closed as PRX-20 says. |
| PRX-14 | **E.** In the INJ-13 scenario, a request from another pod to the app, an HTTP request from the app to another Service and a TLS connection from the app to the controller's webhook Service produce, in the sidecar's logs, access lines with `direction` `inbound` / `outbound` and `protocol` `http1` / `tls` (`sni` = the webhook Service name), and the matching `mimic_http_requests_total{…,status_class="2xx"}` and `mimic_proxy_connections_total{…,protocol="tls"}` series on `/metrics`. |
| PRX-15 | **I.** HTTP/1 byte exactness: for each conversation of a fixed corpus, the bytes the destination receives equal the bytes the client sent and the bytes the client receives equal the bytes the destination sent (whole-stream comparison), on the inbound and the outbound listener. The corpus covers: mixed-case, repeated and reordered headers, 200 response header lines (more than `hyper`'s 100, fewer than the codec's 256); `obs-fold` in a non-framing header; bare-LF line ends; `chunked` with chunk extensions and mixed-case trailers without `TE: trailers`; a close-delimited HTTP/1.1 response; an HTTP/1.0 client with and without `Connection: keep-alive` talking to an HTTP/1.1 destination; `Expect: 100-continue` with `100` then the final response; `103` before `200`; `HEAD` answered with `content-length: 5` and no body, `204`, `304`; 5 pipelined requests in one write; a response sent while no request is pending. Each exchange writes one access line with the method, path, final status and body byte counts of the conversation. |
| PRX-16 | **I.** No framing header is added on HTTP/2: a `POST` and a `PUT` without body and without `content-length` (`END_STREAM` on `HEADERS`) reach the destination without `content-length`, and a request with a body and no `content-length` reaches it without one. |
| PRX-17 | **I.** FIN propagation on HTTP/1: after a complete request, a client `shutdown(Write)` reaches the destination, which reads the complete request then EOF within 1 s; it answers after the EOF and the client reads the whole response, then EOF once the destination closes. With two pipelined requests sent in one write followed by the FIN, the destination reads both requests before EOF. A destination FIN after a close-delimited response reaches the client after the last body byte. When the destination closes without answering after the client's EOF, the access line has `result: client_abort`. |
| PRX-18 | **I.** Resets are propagated as resets: an HTTP/1 client that resets (`SO_LINGER` 0) while its request waits for a silent destination, and one that resets after a FIN, make the destination's socket fail with `ECONNRESET` within 1 s, and the exchange's access line (`result: client_abort`) is written within 1 s; a destination that resets before answering makes the client's socket fail with `ECONNRESET` (`result: upstream_reset`). In an opaque tunnel and in a TLS tunnel, a reset by either side, also during protocol detection, reaches the other as a reset within 1 s. (A peer that already sent its FIN sees the reset as `EPIPE` on Linux, its socket being in `CLOSE_WAIT`.) |
| PRX-19 | **I.** No handshake timeout: against a destination that accepts and never reads or writes, an HTTP/2 client's stream and an HTTP/1 client's request are still pending after 35 s (longer than any `hyper`/`h2` default timer), other connections to other destinations are served meanwhile, and when the client closes, the destination sees its connection closed within 1 s. |
| PRX-20 | **I.** Grace expiry (in-process, grace 1 s): with an idle opaque tunnel, a connection still in detection and an HTTP/1 exchange whose destination never answers, a shutdown leaves the tunnel carrying bytes at 0.5 s; at the end of the grace (+ 1 s) the client and the destination of each see their connection closed, each writes one access line with `result: shutdown`, and one WARN line `grace period over` has `connections: 3`. |
| PRX-21 | **I.** Exchanges with no response head read (`unframed`, PRX-29, and `rejected`, PRX-22) increase `mimic_http_requests_total` with `status_class="none"` and leave the `_count` of `mimic_http_request_duration_seconds` unchanged; their access lines have no `duration_ms` and no `head_ms`. |
| PRX-22 | **I.** An HTTP/2 client that sends a frame breaking the protocol at connection level (e.g. `HEADERS` on an even stream id) gets `GOAWAY` with `PROTOCOL_ERROR`, and one access line is written with `protocol: h2`, `result: rejected`, `authority` = `dst`, and no `status`, `method` or `path`; it is counted with `status_class="none"`. |
| PRX-23 | **U.** Authority sentinels and selection: normalisation maps a value that is not a host name or IP literal to `(invalid)`; `other` and `invalid` are ordinary host names. With a budget of 2, `(invalid)` and the overflow value `(other)` never take a slot, and a real host `other` gets its own label while slots remain. The target authority is the absolute-form authority, else `Host`, for HTTP/1, and `:authority`, else `Host`, for HTTP/2, else `dst`. **I.** An HTTP/2 request with `:authority: a.example` and `host: b.example` is logged and counted under `a.example`. |
| PRX-24 | **I.** Upgrades on the codec: after a WebSocket request answered `101` and a `CONNECT` answered `200`, bytes flow both ways unchanged and are not parsed (bytes that look like HTTP produce no exchange line); client bytes sent after such a request and before its response reach the destination only after the response (checked by the destination's read times). After an upgrade answered `400` and a `CONNECT` answered `403`, the next request on the same connection is forwarded, answered and logged; when the destination then closes the idle connection, the client sees the close within 1 s. |
| PRX-25 | **U.** The open-file target is a pure function of the soft limit, the hard limit and `fs.nr_open`: a finite hard limit above the soft limit gives the hard limit; an unlimited hard limit gives `fs.nr_open` (1048576 when unreadable); a soft limit already at or above the target is left unchanged. |
| PRX-26 | **U.** Codec framing (RFC 9112 §6) on hand-written cases: requests without `content-length`/`transfer-encoding` end after the head; `content-length` and `chunked` (with extensions and trailers) bodies end where the RFC says; a response to `HEAD`, a `1xx`, a `204` and a `304` has no body whatever its `content-length`; `101` and a 2xx to `CONNECT` end the HTTP part; `chunked` as the last coding wins, a response with another last coding or without length is close-delimited; a `1xx` is not the final response and does not consume the pending request. Each message reports its method, path (no query, at most 1024 bytes), target authority inputs, version, status and body byte count. |
| PRX-27 | **U.** Codec properties (`proptest`, at least 10 000 cases each): for arbitrary bytes it never panics and its retained state never exceeds 64 KiB plus a fixed size per direction, whatever the input length; feeding the same bytes in any split into reads yields the same events; for generated valid conversations (random heads, header case and order, `content-length`/`chunked` bodies with extensions and trailers, pipelining, `1xx`, `HEAD`/`204`/`304`, close-delimited ends) the reported message boundaries and body byte counts equal the generated ones. |
| PRX-28 | **C.** A `cargo-fuzz` target feeds arbitrary byte pairs (client side, destination side) to the codec; CI runs it for 60 s on every PR that touches the codec and for 10 min weekly; a crash, a panic, an out-of-memory or a retained state over the PRX-27 bound fails the job and its input is kept as an artefact. |
| PRX-29 | **U.** Each ambiguous or unparsable input of §4.1 "Unframed HTTP/1" (both `content-length` and `transfer-encoding`; differing or non-decimal `content-length`; request `transfer-encoding` not ending in `chunked`, or with HTTP/1.0; `obs-fold` in a framing header; a bare CR; a header line without `:`; an invalid or over-long chunk size; a head, chunk-size line or trailer section over its limit) gives `unframed` at the first byte that decides it. **I.** For a request with both `content-length` and `transfer-encoding`, a header line over 64 KiB and a head of 300 lines, the destination receives the client's bytes unchanged, the client receives the destination's answer unchanged, one access line has `result: unframed` (with `method`/`path` when known, no `status`), and later HTTP on that connection produces no exchange line. |
| PRX-30 | **I.** Bounded read-ahead: against a destination that reads everything and never answers, a client that pipelines 100 requests in one write has exactly 32 of them delivered to the destination; each response the destination then sends lets one more through, and all 100 are eventually answered in order. A client that sends a 10 MiB header line without a line end has all its bytes delivered to the destination unchanged and gets one `unframed` line (its memory bound is PRX-27's). |
| PRX-31 | **I.** With `MIMIC_MAX_CONNECTIONS=2`, a third connection is accepted by the kernel but not served until one of the two ends, then served, and a full inbound side does not delay an outbound connection; an invalid value exits with code 2 (FND-01). A client that sends an incomplete prefix of a protocol (`GE`) and goes silent is reset after the stall duration (injectable; 30 s in production) while a connection that sent nothing is not. |

### 12.4 F3 — TLS interception

Levels as in §12.2; **U** also covers unit tests next to the code of a crate when the logic
needs a crypto crate that `mimic-core` must not depend on. Sidecar integration tests use a
fake sidecar API (a local HTTPS server with a test CA) and destinations with certificates
from test CAs placed in a temporary `MIMIC_TRUST_DIR`.

| ID | Criterion |
|---|---|
| TLS-01 | **U.** The ClientHello parser (`mimic-core`) returns the SNI, the ALPN protocols in order and whether TLS 1.2 or 1.3 is offered (`supported_versions`, else `legacy_version`); an SNI that is not a valid DNS name (300 bytes, an empty label, a space) is reported as invalid and not returned, also when the ClientHello is split over reads and records; it never panics and never reads past the buffer for arbitrary input (property test, extends PRX-01). |
| TLS-02 | **U.** The interception decision is a pure function of the ClientHello facts, the destination, the exclusions, the remembered set and whether a valid intermediate exists, and follows the §5 table in its order for each row (an excluded and remembered destination is `excluded`; a remembered one is passed through even without CA; an invalid SNI is `unsupported` even when the destination IP is excluded or remembered). For `intercept` it returns the ALPN offer: the client's list restricted to `h2` and `http/1.1`, client order. |
| TLS-03 | **U.** Exclusions: an exact host matches itself case-insensitively and with a trailing dot; `*.example.com` matches `a.example.com` and `a.b.example.com`, not `example.com` nor `badexample.com`; an IP literal (v4, v6) matches only a ClientHello without SNI to that destination IP. Parsing `MIMIC_TLS_EXCLUDE` rejects empty entries, `*` alone, `a.*.com`, invalid names and more than 256 entries. **I.** Such a value exits the sidecar with code 2 (FND-01). |
| TLS-04 | **U.** The bounded memory used for remembered destinations and leaves keeps at most its capacity (4096), forgets the oldest insertion first, keeps the first reason of a key inserted twice, and gives the same contents for the same sequence of inserts. |
| TLS-05 | **U.** Intermediate lifecycle as a function of the current time (passed in): no intermediate, an expired one or one not yet valid is unusable (`no_ca`); renewal is due when less than half the validity remains; the backoff after failures is 1, 2, 4 … 60 s and resets after a success; a leaf's validity equals the intermediate's. |
| TLS-06 | **U.** The pod-name check of §7.2 accepts `pay-7d9f8b6c4d-x2k9p` for `pay`, rejects it for `pay-7d9f8b6c4d` and for `pa`; with deployments `a` and `a-b`, `a-b-6d4c8f9b7-xxxxx` is accepted only for `a-b`; a name whose generated prefix was truncated to 58 characters is accepted for its long deployment name; a hash with a character outside the alphabet is rejected. |
| TLS-07 | **U** (controller). The same seed and `<namespace>/<deployment>` give the same root key and subject; another deployment, namespace or seed gives another key. The root is `CA:true`, `keyCertSign`, ECDSA P-256, valid from seed creation − 1 h for 10 years. An intermediate issued for a P-256 public key verifies under the root with `pathLen:0`, `CA:true`, `keyCertSign`, valid from issuance − 1 h for 25 h (time passed in). The root and the intermediate have a Subject Key Identifier; the intermediate's Authority Key Identifier equals the root's. |
| TLS-08 | **I** (controller, fake API server). With the seed Secret empty, the controller writes a 32-byte `seed` and its `created` time and becomes ready only after (and after the INJ-11 conditions), with `mimic_tls_seed_loaded 1`; two replicas started together end with one seed; a restart keeps it; after the Secret's seed is replaced, intermediates are signed by the new root within 60 s (+ 5 s). The seed value appears in no log line. |
| TLS-09 | **I** (controller, fake API server answering `TokenReview`). `POST /v1/tls/intermediate` answers as §7.2 says: `200` with an intermediate for the posted key that chains to the root of (token namespace, requested deployment); `401` without token, with an unauthenticated token and with a token for another audience (the `TokenReview` is sent with `audiences: [mimic.platform.io]`); `403` for a pod name of another deployment and without bound-pod claim; `400`, `413`, `405`; `503` when the `TokenReview` call fails. The listener applies the INJ-19 limits with its own budget. |
| TLS-10 | **I** (controller). Each sidecar API request writes one `sidecar_api` line with the §7.2 fields and increments `mimic_sidecar_api_requests_total{endpoint,result}`; a canary token and the posted key appear in no log line and no metric. |
| TLS-11 | **U.** The injection patch adds what §7.1 "TLS trust wiring" lists, byte-identical for the same input: the two volumes (token audience, expiry, path), the sidecar mounts and variables, one `mimic-trust-<n>` per distinct app image in first-appearance order with its image, pull policy, command, security context, resources, mount and variables; in every app container and app init container the read-only `subPath` mount of its image and the five variables, except those its `env` defines; a literal `JAVA_TOOL_OPTIONS` gets mimic's options prepended, a `valueFrom` one is untouched. No mimic container mounts an app volume; app containers get no `mimic-token` mount. The new `name-conflict` cases skip the pod. |
| TLS-12 | **U.** `MIMIC_TLS_EXCLUDE` is the sorted, deduplicated union of `spec.tls.exclude` of every `MimicWorkload` naming the deployment (absent when none); `MIMIC_TLS_CA` is the deployment's root, `MIMIC_CONTROLLER_URL` and `MIMIC_CONTROLLER_CA` as §7.1 says. **I.** An admission while the seed is not loaded gets `allowed: false`. |
| TLS-13 | **I** (`mimic-trust`, temporary root directory). With a Debian-style store, a RHEL-style store, an image whose own `SSL_CERT_FILE` and `NODE_EXTRA_CA_CERTS` name extra files, and an image without store, it writes the five files of §7.1 with the documented contents and order, mode `0644`, duplicates removed, unparsable blocks skipped and counted; the PKCS12 file lists every bundle certificate as a trusted entry readable with password `mimic` (read back with the `p12-keystore` crate). It logs one JSON `trust bundle written` line (WARN `no image trust store` in the last case), exits 0; 1 when the output is not writable; 2 when `MIMIC_TRUST_CA` is missing or not a certificate. |
| TLS-14 | **C.** `mimic-trust` in the `mimic` image (amd64 and arm64) is statically linked (no `PT_INTERP`, no `DT_NEEDED`); `scripts/check-image.sh` runs it in a `busybox:musl` container and in an empty (`scratch`) image with a store copied in, and both exit 0 with a bundle. |
| TLS-15 | **I** (sidecar, fake sidecar API). With `MIMIC_TLS_CA` set, the sidecar is ready before it has an intermediate; it reads the token file for each request (a changed file sends the new token), posts its public key, and starts intercepting once it holds an intermediate that chains to `MIMIC_TLS_CA` (`mimic_tls_intermediate_expiry_timestamp_seconds` set); before that, and when the API fails, outbound TLS is passed through (`no_ca`) and failures are retried with the TLS-05 backoff (`result="error"`). An intermediate from another root is refused: ERROR `tls ca mismatch`, `result="mismatch"`, no interception. A 4 s intermediate is renewed about 2 s after issuance. A canary token appears in no log line, and no line contains `PRIVATE KEY`. A missing `MIMIC_CONTROLLER_URL` with `MIMIC_TLS_CA` set exits with code 2. |
| TLS-16 | **I.** The sidecar's `/proc/<pid>/limits` shows a core file size of 0 and its `/proc/<pid>/environ` is owned by root (non-dumpable), once it has started. |
| TLS-17 | **I.** Interception, outbound: a client that trusts `MIMIC_TLS_CA` connects with SNI `api.test` to a destination whose certificate for `api.test` comes from a CA in a `trust/0/image-ca.pem`; the client sees a leaf for `api.test` issued by the intermediate, within its validity, whose Authority Key Identifier equals the intermediate's Subject Key Identifier, and the destination sees SNI `api.test`. Over it, the PRX-15 corpus keeps its byte equality on the decrypted streams; with ALPN `h2` (offered `h2,http/1.1`, destination supporting `h2`) the PRX-04 concurrent streams and unary gRPC pass unchanged and the client's negotiated ALPN equals the destination's choice; a client without ALPN gets HTTP/1.1. Exchange lines have `tls: true`, and `Host: api.test:443` gives `authority` `api.test`. A ClientHello without SNI to `127.0.0.1` gets a leaf with that IP SAN. Two connections with the same SNI get the same leaf until the intermediate changes. |
| TLS-18 | **I.** A destination whose certificate is self-signed, expired, for another name, or that only speaks TLS 1.1 is passed through: the destination receives the client's ClientHello byte for byte, the client sees the destination's own certificate (or its own failure, as without mimic); one `connection` line `protocol: tls`, `interception: upstream_untrusted`; one WARN `tls passthrough` per destination; the next connection is passed through without the sidecar's own handshake (the destination accepts one handshake from the sidecar in total); `mimic_tls_passthrough_destinations` counts it. A destination that resets during the sidecar's handshake resets the client within 1 s and is not remembered. A root file that does not parse is logged (WARN `tls roots skipped`). |
| TLS-19 | **I.** A destination that requests a client certificate is passed through (`client_cert_requested`): a client presenting its certificate completes mTLS with the destination, which sees that certificate; it is remembered. |
| TLS-20 | **I.** A client that trusts only the destination's CA (not `MIMIC_TLS_CA`) fails its first handshake (it rejects the mimic leaf), the destination is remembered as `client_rejected` with one WARN `tls passthrough`, and the client's next connection succeeds with the destination's own certificate. A client that resets the connection during the handshake, or that stalls, is not remembered. |
| TLS-21 | **I.** With `MIMIC_TLS_EXCLUDE=*.excluded.test`, a connection with SNI `a.excluded.test` is passed through without any handshake from the sidecar (`interception: excluded`), and `api.test` is still intercepted. A ClientHello offering only TLS 1.1, or ALPN `imap` only, is passed through as `unsupported` and not remembered. An inbound TLS connection is never intercepted and its line has no `interception` field. |
| TLS-22 | **I.** Close and reset on the decrypted path: a client `close_notify` + FIN after a complete request reaches the destination as `close_notify` + FIN after the request bytes, and the destination can still answer (PRX-17); a TCP reset by either side reaches the other as a reset within 1 s (PRX-18); at the end of the shutdown grace an intercepted exchange writes `result: shutdown` (PRX-20). |
| TLS-23 | **I.** After a known set of connections (intercepted, excluded, `no_ca`, `unsupported`, `upstream_untrusted`, `client_rejected`), `/metrics` shows the exact `mimic_tls_connections_total{interception}` values, `mimic_tls_passthrough_destinations`, and intercepted connections under their decrypted protocol with the SNI as `authority` in `mimic_proxy_connections_total` (§9). The PRX-08 canaries (header, cookie, query, bodies) sent over an intercepted connection appear in no log line. |
| TLS-24 | **E.** The chart creates the empty seed Secret, the Service and NetworkPolicy port 8444, and the RBAC of §11: `kubectl auth can-i` as the controller is `yes` for `create tokenreviews` and `get`/`update` of the seed Secret, `no` for other Secrets and pods; after install the Secret holds a seed; `helm uninstall` removes it. The CRD accepts `spec.tls.exclude: [api.example.com, "*.corp.example", 10.0.0.1]` and rejects `*`, `a.*.com` and 257 entries. |
| TLS-25 | **E.** In an `enabled` namespace, four client Deployments built by the e2e from `curlimages/curl`, `python:slim` (`urllib`), `node:slim` (`fetch`) and `eclipse-temurin` (JDK `HttpClient`), each with the e2e test CA installed in the image's system store, call an in-cluster HTTPS Service whose certificate comes from that CA: each call returns 200, the sidecar writes an exchange line with `tls: true` and `status: 200`, and the certificate the client saw is issued by the mimic intermediate. A call to an HTTPS Service signed by an unknown CA fails in the client as without mimic (curl exit 60) and the sidecar logs `upstream_untrusted`. A `MimicWorkload` with `spec.tls.exclude` naming the Service host makes the next pod's connections `excluded`. |
| TLS-26 | **E.** A client container that defines `SSL_CERT_FILE` in its spec to a file holding only the e2e CA keeps that value (no mimic variables for it); its first call fails, the sidecar logs one WARN `tls passthrough` with `reason: client_rejected`, and its second call returns 200 with the Service's own certificate. A sidecar token presented for another deployment's name gets `403` from the sidecar API. |
| TLS-27 | **I** (sidecar). A destination that the app image's trust accepts is intercepted although the first handshake with the client is a reset: a client that resets during the handshake is not remembered (`mimic_tls_passthrough_destinations 0`, no WARN), one that sends an alert is. An intercepted connection to a server-first protocol over TLS (the destination sends a banner, the client waits) receives the banner. A failing `publish_trust` (unwritable `MIMIC_TRUST_DIR`) leaves outbound TLS passed through (`no_ca`) and logs ERROR `trust volume not published`. |
| TLS-28 | **I** (controller). The sidecar API rejects (400) a posted key that is not a point of the P-256 curve; with a budget of 2 `TokenReview` calls per source the third request from one source within the window gets 503 without a call to the API server while another source is still served, the budget refills with time, and the table of sources keeps at most its capacity (oldest forgotten first); a failing `TokenReview` writes exactly one `sidecar_api` log line, carrying `error`. **U.** A union of `spec.tls.exclude` of 300 distinct entries over several `MimicWorkload`s is cut to its first 256 in sorted order. |
| TLS-29 | **I** (sidecar CA client). An answer larger than 64 KiB is refused while it streams (the client does not buffer more than the cap plus one chunk). **I** (`mimic-trust`). A store file over 8 MiB is ignored like an unreadable one. |

### 12.5 F4 — Ingress endpoint discovery

Levels as in §12.2. Sidecar integration tests run the real binary against a local test app
with a fake original destination (as in F2) and read `GET /discovery` from loopback. A
*canary* is a secret value unique to the test, searched for in every output.

| ID | Criterion |
|---|---|
| DSC-01 | **U.** Segment classification: `42`, a UUID in upper and lower case, a ULID, `5f2b9c1d` (hex with a digit), `2026-11-01`, `2026-11-01T10:00:00Z`, `a1b2c3d4e5f6g7h8i9` (16+ with letter and digit), `alice@example.com` and a JWT are parameters; `v1`, `api`, `users`, `deadbeef` (no digit), `health`, an empty last segment and `%41` (not decoded) are literals. Parameter names follow §6 (`/users/42/orders/7` → `/users/{users_id}/orders/{orders_id}`, `/42` → `/{param1}`, a repeated literal gets `{param<n>}`), and a gRPC path is never templated (it is still redacted: DSC-06). |
| DSC-02 | **U.** Absorbing nodes: with `MIMIC_DISCOVERY_MAX_LITERALS` = 3, `/u/a`, `/u/b`, `/u/c` stay three endpoints, a fourth literal turns them into one `/u/{u_id}` whose counters, schemas and samples are the merge of the four, later literals map to it, and a `/u/42` parameter and the literals share it; siblings of `/u` are untouched. For any set of paths (`proptest`, ≥ 10 000 cases), every permutation of the arrival order gives the same templates. |
| DSC-03 | **U.** Schema inference and merge follow §6: `type` sets with `null`; `integer` only while every number is integral; each `format` only when every value has it; `required` = present in every object; array `items` over the first 256 items; an object with 129 keys becomes a map; level 17 is `{}`. Query: keys percent-decoded, a repeated key is an `array`, a key missing once is not `required`, at most 64 keys (first in byte order). Headers: names in lower case, the skipped names of §6 absent, at most 64. `application/x-www-form-urlencoded` bodies get the same schema as the equivalent query; `text/plain`, `application/grpc` and `application/octet-stream` are opaque (media type and size). |
| DSC-04 | **U.** Volatility: a field with values `a`, `b`, `c` is volatile; `a`, `b` (n = 2) is not; 10 observations of 2 values are not; 1000 distinct values are, and 1000 observations of 100 values are not (fixed data, deterministic hash). The sketch holds at most 32 hashes; merging two sketches equals the sketch of the union. At most 128 fields are tracked (first in byte order). A redacted field is marked `redacted`; `n` counts an exchange once, whatever the repeats inside it (a field in 10 array items, one call: `n` = 1). |
| DSC-05 | **U.** Redaction by name: each built-in name of §6, and names containing them in any case with `-`/`_` (`X-Api-Key`, `client_secret`, `refreshToken`, `Session-Id`) or glued into one token (`JSESSIONID`, `sessionid`, `csrftoken`, `xapikey`, `clientsecret`, `dbpassword`), while `company`, `author`, `panel` and `passenger` are kept, as a header, query key, form field and JSON key (also nested, and with an object or array value, which becomes the marker as a whole), gives `<redacted>`; `Authorization: Bearer x` and `bearer x` → `Bearer <redacted>`, `Digest realm=r` keeps `Digest`, `Authorization: NotAScheme x` and `Proxy-Authorization: <canary> x` → `<redacted>`, `Cookie: a=1; b=2` → `a=<redacted>; b=<redacted>`, `Set-Cookie: s=1; Path=/; HttpOnly` → `s=<redacted>; Path=/; HttpOnly`; a name of `MIMIC_DISCOVERY_REDACT_NAMES` is redacted; in a URL-valued header (`Location: https://h/cb?code=C&access_token=T`, `Referer`, `Link: <u?api_key=K>; rel=next`) the query parameters (and fragment) are redacted by name; `id`, `name` and `amount` are not; neither are `company`, `author`, `panel`; `tokens`, `auth.token`, `authToken` and `Authorization` are, extra names follow the same token rule, and `Link: <cb?token=T>` (relative URL) is redacted. The URL redactions are counted under `header` and only for the 64 kept headers. |
| DSC-06 | **U.** Redaction by value, in a header value, a query value, a path segment, a JSON string and (cards) a JSON number: a JWT, a PEM private key, `Bearer`/`Basic` credentials, `4111 1111 1111 1111` and `4111-1111-1111-1111`, an `AKIA` key, each listed token prefix, and the password of `https://u:p@h/` (also percent-encoded in a path segment or a query value: `/reset/%65yJ…`) become `<redacted>`, also inside a URL in a header value (`Location: https://h/reset/<JWT>`); near misses are kept (`4111111111111112` fails Luhn, three base64url parts whose first does not decode to JSON with `alg`, `AKIA` + 15). A redacted path segment is a parameter. The marker is the same for every secret. |
| DSC-07 | **U.** No leak (`proptest`, ≥ 10 000 cases): generated exchanges carrying a canary under a listed name or in a detected form, at a random place (path, query, header, form or JSON body at any depth, request or response), give a serialised report that contains neither the canary nor its XXH3 hash, in decimal or hex. |
| DSC-08 | **U.** Samples: at most 20 per endpoint; with 25 candidates of statuses 200, 201, 400, 500, every status is represented, then every (status, request shape, response shape) class, then the lowest content hashes; identical candidates count once; any arrival order gives the same set. A body over 256 KiB, encoded or decoded, and an opaque body are not kept (size and media type are). When the sample budget is full a new sample is rejected (`budget`) while counters and schemas still change. |
| DSC-09 | **U.** Merge algebra (`proptest`, ≥ 10 000 cases): `merge` is commutative and associative; recording a sequence of exchanges one by one equals merging the reports of any partition of it (as several replicas would), counters and histograms included (saturating at `u64::MAX`); a merge of reports over the limits stays within `max_endpoints` and `sample_budget`, commutatively; two reports each below the literal limit at one node absorb it when their union exceeds it. Serialisation is byte-identical for equal reports, keys sorted, `"version": 1`, and it round-trips. |
| DSC-10 | **U.** Latency buckets: 4 per doubling from 1 ms; 0 ms and 1 ms fall in the first bucket, a value on a boundary in the bucket it closes, more than 131 s in the overflow; merge adds bucket counts. |
| DSC-11 | **U.** Bounds: past `MIMIC_DISCOVERY_MAX_ENDPOINTS` new endpoints are refused (`endpoint_limit`) while known ones still update; a `404` or `405` on a path matching a known template is counted on it, else it is `not_found` and creates nothing; a path over 2048 bytes or 32 segments is `invalid`. For arbitrary exchanges (`proptest`) the recorder never panics and its retained state, sample bytes excluded, stays below a bound computed from the limits and the caps of §6 (512 schema nodes per body, 128 volatile fields, 64 keys per query or header set, 128 bytes per name). a name over 128 bytes is recorded as its first 128, cut on a UTF-8 character boundary, and a name that is sensitive only past byte 128 is still redacted. 10 000 distinct wide or deep bodies leave a body schema at 512 nodes or fewer; with the cap, schema merge is commutative and associative and any partition of the exchanges merges to the same schema (`proptest`, wide and deep bodies). |
| DSC-12 | **I.** Inbound HTTP/1.1 and `h2` exchanges to the test app (`GET /users/{1..5}` with JSON responses, `POST /orders` with a JSON body and a `?dry_run=true` query, a `201` and a `400`) appear in `/discovery` under `direction` `inbound`, the app port and their templates, with status counters, histogram, query and header schemas, body schemas and samples; probe requests, outbound exchanges, `unframed` exchanges and a `404` to an unknown path do not; `hosts` lists the `Host` values used. |
| DSC-13 | **I.** Bodies with `content-encoding` `gzip` and `deflate` are decoded and inferred; `br` and `zstd` are `encoding` and their content is not inferred; a 1 KiB gzip body that expands past 256 KiB is `too_large` and the sidecar's RSS does not grow by more than 1 MiB; malformed JSON is `invalid`; each case increments `mimic_discovery_bodies_total` with its `result`. |
| DSC-14 | **I.** `GET /discovery` on the admin port answers 200 `application/json` with the local report to a client connecting from `127.0.0.1` and from `::1`, and 403 to one connecting from the pod's non-loopback address. |
| DSC-15 | **I.** No leak end to end: canaries sent as `Authorization`, `Cookie`, `X-Api-Key`, a query `token=`, a JWT path segment, a card number and `password` in JSON request and response bodies, and in a `x-custom` header value with a `ghp_` prefix, appear neither in `/discovery`, nor on stdout (access lines included: the path shows `<redacted>`), nor in `/metrics`; bytes reach the app and the client unchanged. |
| DSC-16 | **I.** Discovery never slows or alters traffic: with `MIMIC_DISCOVERY_QUEUE=1` and `MIMIC_DISCOVERY_CAPTURE_BUDGET=1Mi`, 2000 concurrent requests with 200 KiB bodies are all answered with the bytes the app sent; `mimic_discovery_exchanges_total` sums to the number of exchanges, with `dropped` > 0, and bodies skipped for budget are counted `budget`. |
| DSC-17 | **I.** Memory: 256 endpoints each receiving 30 distinct 256 KiB JSON exchanges leave the sidecar RSS under 128 MiB with `mimic_discovery_sample_bytes` ≤ `MIMIC_DISCOVERY_SAMPLE_BUDGET`; a 257th endpoint is `endpoint_limit`, with one WARN `endpoint limit reached`. |
| DSC-18 | **I.** Logs (`target` = `discovery`): one INFO `endpoint discovered` per new endpoint with `direction`, `port`, `method`, `template`, and one when a node is absorbed (`template absorbed`, old and new templates); one WARN `sample budget full` when the budget becomes full (not repeated while it stays full); a recorder panic is logged and counted `panic`, later exchanges are still recorded; no line holds a header value, query, body or sample. `mimic_discovery_endpoints` and `mimic_discovery_redactions_total{location,rule}` match the traffic sent. |
| DSC-19 | **I.** An invalid `MIMIC_DISCOVERY_*` value (out of range, bad size suffix, a 65th or invalid redact name) exits 2 before listening (FND-01). |
| DSC-20 | **U.** The injection patch sets `MIMIC_DISCOVERY_REDACT_NAMES` to the sorted, deduplicated union of `spec.discovery.redact` of every `MimicWorkload` naming the deployment, absent when there is none, entries lower-cased. **E.** The CRD accepts `spec.discovery.redact: [x-tenant-key, Pin]` and rejects an entry with `/`, an empty one and 65 entries. |
| DSC-21 | **U.** The access-log path function applies the value detectors of DSC-06 segment by segment and leaves other segments unchanged. **I.** An inbound and an intercepted outbound request to `/reset/<JWT>` write access lines with `path` `/reset/<redacted>`. |
| DSC-22 | **E.** On kind, a 2-replica Deployment in an `enabled` namespace behind a Service gets calls to `/users/<id>` (ids, a canary bearer token, JSON bodies); through `kubectl port-forward` each pod's `/discovery` shows `GET /users/{users_id}` with its response schema and samples, and no canary; a `404` storm on random root paths creates no endpoint. (Amended by CTL-20: from F5 a pod's `/discovery` holds only what it has not shipped yet, so the endpoint, schema and sample checks read the merged report; `/discovery` is still checked for canaries.) |
| DSC-23 | **C.** A `cargo-fuzz` target feeds arbitrary request and response heads and bodies to the recorder (parse, decode-free inference, redaction, merge); CI runs it 60 s on PRs touching `crates/mimic-core/src/discovery/` and 10 min weekly, like PRX-28 (same `ci-ok` path-filter rule). |

### 12.6 F5 — Controller collects and merges endpoints

Levels as in §12.2. Controller integration tests run the real controller binary (or its
routers in-process) against a **real Valkey** (the chart's pinned image, started by the test
harness, or `MIMIC_TEST_VALKEY_URL`; a CI service container) and the fake API server of F3,
which answers `TokenReview` and serves `MimicWorkload`s with their status subresource.
Sidecar integration tests use the fake sidecar API of F3. A *canary* is as in §12.5.

| ID | Criterion |
|---|---|
| CTL-01 | **U.** The structural check of §7.3 accepts every report the recorder produces within the §6 bounds, merged or not (`proptest`, ≥ 10 000 cases), and rejects, each with its own fixed `reason`: a version other than 1; 21 samples; 17 hosts or a host over 1024 bytes; status 99 or 600; a histogram bucket out of range; a schema of 513 nodes, of 17 levels or with a cut level of 17; a 129-byte name; 65 query keys or headers; 129 volatile fields or a 33-hash sketch; a 2049-byte or 33-segment path or template; 17 media types on a side; a 256 KiB + 1 sample body or a sample body for an opaque media type; an unknown field; more than `MIMIC_STORE_MAX_PUSH_ENDPOINTS` endpoints in a direction is `push_limit`, apart from the `400` reasons. `Report::from_json` then the check never panic on arbitrary input (`proptest`, and CTL-22). (Amended by SHR-01: from F8 version 2 is accepted with an outbound scope.) |
| CTL-02 | **U.** The bounded merge equals the §6 `merge` while the result is within the store limits, and below them does not depend on the order or grouping of pushes (`proptest`); two pushes each below the literal limit at a node absorb it when their union exceeds it; counters and buckets at 2^64 − 1 stay there; the result's `pod` is null. Anti-crowding, with `MIMIC_STORE_MAX_ENDPOINTS` = 4: a store of 4 endpoints with 100 exchanges each, merged with a push of 50 endpoints `/!0` … `/!49` of 1 exchange each, keeps the 4 (50 `endpoint_limit` refusals, no eviction); a push of an endpoint with 101 exchanges evicts the stored endpoint with the fewest exchanges, ties broken by key (`evicted` = 1); a new endpoint tying the lowest stored count does not displace it. Samples, with a full budget: a new `5xx` sample of an endpoint holding only `2xx` samples evicts samples of the (endpoint, class) with the most samples until it fits, never the only sample of a class; a new `2xx` sample of that endpoint is refused (`budget`); when room cannot be made without emptying a class, nothing is evicted. The same input always gives byte-identical output; `refused` and `evicted` counts are returned per kind and reason. |
| CTL-03 | **U.** Re-redaction is idempotent and leaves unchanged a report recorded with the same redactor. For reports recorded with a redactor that lacks a name the controller's redactor has (`proptest`, ≥ 10 000 cases), with a canary under that name at a random place (query, header, form or JSON body at any depth, request or response, URL-valued header), the re-redacted report contains neither the canary nor its XXH3 hash in decimal or hex, its sample keys are the hashes of the re-redacted content, and the field is marked `redacted` with the hash of the marker as its only hash, equal to the field of a sidecar that knew the name. A canary as the first word of `Authorization` or `Proxy-Authorization` (an unlisted scheme) in a report recorded by an older sidecar is removed the same way. A canary in a detected form (JWT, card, token prefix) in a report that bypassed the sidecar's redaction is removed the same way from its samples, hosts and path keys (tree literals, gRPC paths: equal keys merge); a volatile field's sketch is dropped when a retained sample shows a detected form at that field and kept otherwise. A schema or parameter key under a redacted name has the schema of the marker, and the volatile fields below it become its field. |
| CTL-04 | **U.** Retention: stamping a push with day `D` sets `last_seen = D` on each endpoint and sample; merge keeps the larger value (commutative, associative); dropping with day `T` and a TTL of `n` days removes exactly the endpoints and samples with `last_seen < T − n` (counted per kind); a sidecar report without `last_seen` parses, and a sidecar never serialises it. |
| CTL-05 | **U.** The key names of §7.3 for namespace `shop` and deployment `pay.v2` are byte-exact; a namespace or deployment name that is not DNS-1123 is refused before any key is built. The sequence decision applies a push whose sequence is above the stored one of its pod and session, or that has none, and reports any other as a duplicate; a session that is not 32 lower-case hex characters and a sequence of 0 or above 2^53 are `invalid`. |
| CTL-06 | **U.** The status of §7.4 is a pure function of `:meta`, the pods' push times, the window and the current time (passed in): byte-identical for the same input; a pod counts exactly when its last push is at or after now − window; without data the counts are zero and `lastPushTime` is absent; `lastPushTime` is RFC 3339 UTC in seconds; directions without endpoints are omitted. |
| CTL-07 | **I.** `POST /v1/workloads/<deployment>/report` answers as §7.2 says: `200` `{"applied": true}` and the stored report equals the bounded merge; the same body again, and a lower sequence of the same session, `200` `{"applied": false}` with `:report` and `:meta` unchanged; a new session of the same pod applies from sequence 1; `401` and `403` as TLS-09; `400` for malformed JSON, a bad session or sequence, a missing `Content-Length` and one CTL-01 violation; `405`; `413` when `Content-Length` is over `MIMIC_SIDECAR_API_MAX_REPORT`, received by the client before it has sent any body byte; `507` for a report over `MIMIC_STORE_MAX_PUSH_ENDPOINTS` (= 3 in the test) endpoints in a direction, with nothing stored, and for a new deployment once `MIMIC_STORE_MAX_DEPLOYMENTS` (= 2 in the test) deployments of the namespace have data, while those two still apply; `503` with `Retry-After` while Valkey is stopped, when the `TokenReview` fails, and when two concurrent bodies exceed `MIMIC_SIDECAR_API_BODY_BUDGET`. An unauthenticated request declaring a 1 GiB body is answered `401` while the client has sent only its headers. |
| CTL-08 | **I.** Rate limit and review cache: with `MIMIC_SIDECAR_API_RATE=5`, 30 requests sent at once from one source get 10 answers other than `429` and 20 `429` with `Retry-After: 1`, the fake API server receives no `TokenReview` for the `429`s, a second source is served meanwhile, and 1 s later 5 more requests from the first pass. Two pushes with the same token within 60 s cause one `TokenReview`; another token causes its own; a failed review is not cached (the next request with that token is reviewed again). |
| CTL-09 | **I.** Applied once across replicas: two controller processes share one Valkey; 16 simulated sidecars (pods of 2 deployments) each post 50 generated pushes to random replicas, 30 % of them posted twice and 10 % posted to both replicas at once, with limits that are not reached. For each deployment the stored `:report` is byte-identical to the bounded merge of its distinct pushes, `:meta` `exchanges` is their sum, and `mimic_store_writes_total{result="conflict"}` is above 0 on a replica (contention happened). Passes 3 runs in a row. |
| CTL-10 | **I.** Keys and expiry: after a push exactly the keys of §7.3 exist for its deployment, with their types and fields; with `MIMIC_STORE_TTL=10s` every key and both index members have an expiry of at most 10 s, refreshed by each applied push (a duplicate refreshes only its `:pods` field); 10 s (+ 2 s) after the last push the deployment's keys and index members are gone, and the `:pods` and `:seq` fields expire after min(24 h, TTL). |
| CTL-11 | **I.** Nothing sensitive reaches Valkey: pushes carrying the DSC-15 canaries unredacted (as a faulty sidecar would send them) and a canary under a name that only the `MimicWorkload`'s `spec.discovery.redact` lists are applied, and no value of any `mimic:v1:*` key (dumped with `SCAN` and `GET`/`HGETALL`/`ZRANGE`) contains a canary, its XXH3 hash or the bearer token; nor does stdout or `/metrics`. |
| CTL-12 | **I.** Each push writes one `sidecar_api` line with the §7.2 fields (`endpoint` `/v1/workloads/{deployment}/report`, `session`, `sequence`, `bytes`, `result`, `reason` for `invalid`), at DEBUG for `rate_limited`; after a known sequence of pushes `mimic_sidecar_api_requests_total{endpoint,result}`, `mimic_store_pushes_total`, `mimic_store_writes_total`, `mimic_store_refused_total`, `mimic_store_evicted_total`, `mimic_store_report_bytes` and `mimic_store_write_duration_seconds` have the expected values; one WARN `store limit reached` per deployment and reason and one WARN `store evicted` per deployment and kind; no line and no series contains a path, template, header, sample, token or report content. |
| CTL-13 | **I.** Read API: `GET /v1/workloads`, `/v1/workloads/<ns>/<dep>` and `/v1/workloads/<ns>/<dep>/report` on `MIMIC_UI_ADDR` answer as §7.5 (list sorted, pods map, report bytes equal to `:report`); `404` for a deployment without data, `400` for an invalid name, `405` for `POST` and `DELETE`, `503` while Valkey is stopped, `500` with one ERROR `stored report unreadable` for a corrupted `:report` (also refused by a push, `503`, without overwriting it). With `MIMIC_UI_ADDR=0.0.0.0:<port>` a request from the host's non-loopback address gets `403`; a loopback request with `Host: evil.example`, `Host: localhost.evil.example` or no `Host` gets `403`, and `localhost:8080`, `127.0.0.1` and `[::1]:8080` are served. |
| CTL-14 | **I.** An invalid value of each variable added in §10.2 (out of range, bad suffix, `MIMIC_SIDECAR_API_BODY_BUDGET` below `MIMIC_SIDECAR_API_MAX_REPORT`, `MIMIC_STORE_MAX_PUSH_ENDPOINTS` above `MIMIC_STORE_MAX_ENDPOINTS`, an unparsable `MIMIC_UI_ADDR`) and of `MIMIC_DISCOVERY_PUSH_INTERVAL` exits 2 before listening (FND-01); `1m`, `2h` and `30d` parse as durations. |
| CTL-15 | **I.** Shipping: with `MIMIC_DISCOVERY_PUSH_INTERVAL=1s` and exchanges to the test app, the sidecar posts to `/v1/workloads/<MIMIC_WORKLOAD>/report` bodies with one 32-hex session and sequences 1, 2, …; the exchanges of all posts sum to the exchanges recorded; an interval without exchanges posts nothing; the token file is read for each post; after a `200`, `/discovery` holds only exchanges recorded since; a restarted sidecar posts a new session from sequence 1. Without `MIMIC_CONTROLLER_URL` nothing is posted and one INFO `report shipping disabled` is written. |
| CTL-16 | **I.** Retries: while the fake API answers `503`, resets the connection or stays silent past 30 s, the sidecar posts the same bytes with the same sequence again with the 1, 2, 4 … s backoff, and the exchanges recorded meanwhile go into the next sequence, posted only after the pending one is acknowledged; `429` with `Retry-After: 3` delays the retry by at least 3 s; `413` is followed by the same sequence without samples; `400` drops the pending report with one WARN `report rejected` and the next sequence follows. Samples of the pending and current reports never exceed `MIMIC_DISCOVERY_SAMPLE_BUDGET`. `mimic_discovery_pushes_total{result}` and `mimic_discovery_last_push_timestamp_seconds` follow; one WARN `report push failed` per failure streak (a streak ended by a dropped report, `400`, `405` or `507`, included: the next failure logs again) and one INFO `report push recovered`; a `Retry-After` over 300 s waits 300 s; a `200` with a body that is not `{"applied": <bool>}` is a failed attempt; `mimic_discovery_sample_bytes` and `mimic_discovery_endpoints` cover the pending and the current report; no line holds the token. |
| CTL-17 | **I.** At `SIGTERM`, a sidecar with a pending report and unshipped exchanges posts the pending then the current report and exits 0 within `MIMIC_SHUTDOWN_GRACE`; the fake API received every exchange once. With the fake API silent, it exits within the grace (+ 1 s) with one WARN `report not shipped` whose `exchanges` is the number lost. Run as the binary (`SIGTERM` to the process, traffic redirected as in the pod), with a connection that never completes holding the drain, the report is still posted within the grace: the drain stops at three quarters of it. |
| CTL-18 | **I.** Status: within `MIMIC_STATUS_INTERVAL` (+ 1 s) of a push, every `MimicWorkload` naming that deployment gets the §7.4 `status.discovery` through the status subresource by server-side apply with field manager `mimic-controller`; an unchanged status sends no request; a `MimicWorkload` of a deployment without data gets the empty status; while Valkey is stopped no status request is sent and one WARN `status not updated` is written (applies already queued in a pass, from reads that succeeded before the outage, still finish; no deployment is read after the failure); `mimic_status_updates_total` counts the applies. |
| CTL-19 | **E.** The CRD has the status subresource and schema of §7.4 and the printer columns `Mode`, `Endpoints`, `Pods`, `Last push`; `kubectl auth can-i` as the controller is `yes` for `patch mimicworkloads/status` and `no` for `update mimicworkloads`, `get pods` and `list replicasets`; Valkey answers `CONFIG GET maxmemory` with 75 % of its memory limit and `maxmemory-policy` `noeviction`; no Service and no NetworkPolicy port exposes `MIMIC_UI_ADDR`. |
| CTL-20 | **E.** On kind with the shared apps: `users` (B, 2 replicas) in an `enabled` namespace with a `MimicWorkload`, `front` (A) calling it, and the client sending 40 `GET /front/users/<id>` (numeric ids, a canary bearer token) and 5 `PUT` with a canary in the JSON body, spread over both pods. Within 2 push intervals, through `kubectl port-forward` to a controller pod's `MIMIC_UI_ADDR`, `/v1/workloads/<ns>/users/report` has `GET /users/{users_id}` with as many `200`s as the client got for its `GET`s (probe requests are not recorded) and `PUT /users/{users_id}` with 5, with response schemas and samples; `/v1/workloads/<ns>/users` lists both pods; within `MIMIC_STATUS_INTERVAL` more the `MimicWorkload` shows `endpoints.inbound: 2`, `pods: 2` and a `lastPushTime`, also in `kubectl get mimicworkload`. No canary is in the report, in any Valkey value (`valkey-cli` dump), in the controller's logs or in the pods' `/discovery`. |
| CTL-21 | **E.** Churn: with traffic from the client before, during and after `kubectl rollout restart deployment/users`, and one controller pod deleted while pushes are flowing, the merged `200` count of `GET /users/{users_id}` is at least the number of `200`s the client received (no exchange lost at `SIGTERM`) and at most the number the users pods answered, from their access lines (none counted twice; a rollout cutting a connection after the pod answered may cost the client its answer and the sidecar its exchange, never more than that gap); with `MIMIC_STATUS_POD_WINDOW=30s` (set by the e2e), `status.discovery.pods` comes back to 2, with the pods of the rollout listed by the read API, after the old pods' window while traffic still flows (a sidecar pushes only when it recorded exchanges, so without traffic the window empties), and the endpoints are unchanged. |
| CTL-22 | **C.** The DSC-23 fuzz target also feeds arbitrary bytes to `Report::from_json`, the structural check, re-redaction and the bounded merge with a fixed valid report: no panic, and every merge result passes the check. Same CI schedule and `ci-ok` path-filter rule as DSC-23. |

### 12.7 F6 — Discovery UI (OpenAPI 3.1 + Swagger UI)

Levels as in §12.2. Unit tests of §7.6 run on reports built by the recorder and the bounded
merge (the generators of DSC and CTL tests). Documents are validated against the official
OpenAPI 3.1 schema (`https://spec.openapis.org/oas/3.1/schema-base/2022-10-07`, vendored as a
test fixture with the three schemas it references, unmodified, Apache-2.0, in `crates/mimic-core/tests/fixtures/oas-3.1/` with their source commit, SHA-256 and licence in its `README.md`) with the `jsonschema` crate as a dev-dependency (default features off: no
network). Controller integration tests run against a real Valkey as in §12.6. The browser
checks use a pinned headless Chromium image (`images/Dockerfile` stage `e2e-chromium`), run
on the host network against the port-forward, with `--host-resolver-rules` that resolve no
name but `localhost`. A *canary* is as in §12.5.

| ID | Criterion |
|---|---|
| UI-01 | **U.** Determinism and validity: for every generated report (`proptest`, ≥ 10 000 cases) the index, the endpoint list and every document of §7.6 are byte-identical across two calls, and equal for reports merged from the same pushes in another order or grouping (below the store limits); every document validates against the OpenAPI 3.1 schema-base, has `openapi` `3.1.1`, the `info.title` and `info.version` of §7.6, and no `$ref` member other than a key of a `properties` map or inside an example `value`. |
| UI-02 | **U.** Paths: `/users/42` and `/users/7` give the key `/users/{users_id}` with a required path parameter `users_id` of schema `{"type": "string"}`; UUID sample values give `format: uuid`, mixed values none; `/` and `/users/` (trailing slash) are their own keys; a literal `a{b}` is written `a%7Bb%7D`; `GET` … `TRACE` are lower-case operations and `PROPFIND` is under `x-mimic-methods`; the same template on ports 8080 and 9090 gives two documents `inbound-8080` and `inbound-9090`, each holding it once; a gRPC path `/pkg.Svc/Get` is a key as it is, with `tags: ["pkg.Svc"]` and `x-mimic-grpc: true`. The index lists the documents sorted by id with their endpoint counts; the endpoint list follows the report's endpoint order and names each endpoint's document. |
| UI-03 | **U.** Parameters: a query key in every exchange is `required`, one in some is not, a repeated one is an `array` with `items`; request headers are header parameters in byte order without `accept`, `content-type` and `authorization`; parameters come in the order path, query, header; each has `examples` named `sample-<16 hex>` after the sample keys, one per distinct value; a volatile field has `x-mimic-volatile: true` and a redacted one `x-mimic-redacted: true`, with the marker as its only example value. |
| UI-04 | **U.** Bodies and responses: a JSON request body gives `requestBody.content["application/json"]` with the §6 schema and the samples' bodies as examples; a form body gives an object schema and an object example (a repeated field is an array); an opaque media type has no schema, with `x-mimic-count` and `x-mimic-bytes`; `required` follows §7.6. Statuses `200`, `404` and `299` give responses with descriptions `OK`, `Not Found` and `Status 299` and their `x-mimic-count`; a response holds the media types and headers (without `content-type`) seen in its own samples, with the endpoint-wide schema; a status without a sample has a description only. |
| UI-05 | **U.** Samples with `authorization: Bearer <redacted>` and `Basic <redacted>` give `components.securitySchemes` `http_bearer` and `http_basic` and both requirements, in that order; a bare `<redacted>` gives the `apiKey` scheme `authorization`; a scheme outside the registered list of §6 (a canary, `Bea_rer`) is ignored and appears nowhere in the document. `servers` holds `http://<host>` for the scope's valid hosts, sorted, at most 16, and never `(invalid)` or a host with the marker; none qualifying gives no `servers`. A histogram of 100 exchanges at 10 ms gives `p50`, `p90` and `p99` equal to the upper bound of the 10 ms bucket, rounded up; one in the overflow bucket gives `null`; `x-mimic-last-seen` is `YYYY-MM-DD` of the stamped day. |
| UI-06 | **U.** No recorded content in Markdown: for reports with a canary at every recordable place (path literal, gRPC path, query key and value, header name and value, JSON key and value, form field, host; `proptest`), no `description`, `summary` or `title` value and no tag `description` of any document contains the canary, and `info.description` is the constant text. A report re-redacted by the controller (§7.3) renders to documents in which no DSC-15 canary appears. |
| UI-07 | **I.** Routes on `MIMIC_UI_ADDR`: `/v1/workloads/<ns>/<dep>/endpoints`, `/openapi` and `/openapi/inbound-<port>` answer `200` with exactly the §7.6 output for the stored `:report` and `:meta` `version`; `400` for the ids `inbound-0`, `inbound-65536`, `inbound-08080` and `foo`; `404` for an absent port and for a deployment without data; `503` while Valkey is stopped; `500` with one ERROR `stored report unreadable` for a corrupted `:report`. `HEAD` on every route of §7.5 (F5 routes, pages and assets included) answers the status and headers of `GET` with an empty body; `POST` and `DELETE` answer `405` with `Allow: GET, HEAD`; the CTL-13 `403` cases (non-loopback peer, `Host: evil.example`, no `Host`) hold on every new route. |
| UI-08 | **I.** Headers: every answer of the listener (a page, a JSON route, a vendored asset, `308`, `400`, `403`, `404`, `405`, `500`, `503`) carries the exact `Content-Security-Policy` of §7.5, `X-Content-Type-Options: nosniff`, `Referrer-Policy: no-referrer`, `X-Frame-Options: DENY`, `Cross-Origin-Opener-Policy: same-origin`, `Cross-Origin-Resource-Policy: same-origin` and `Cache-Control: no-store`, except `public, max-age=31536000, immutable` on `swagger-ui/<version>/` assets; no answer has an `Access-Control-Allow-*` header, also for a request with an `Origin`. |
| UI-09 | **I.** Pages and assets: `GET /` is `308` to `/ui/`; `/ui/`, `/ui/workloads/<ns>/<dep>` and `/ui/workloads/<ns>/<dep>/openapi/<id>` are `200` `text/html; charset=utf-8`, have no inline script, no `on*` attribute and no `style` attribute, and load scripts and stylesheets from `/ui/assets/` only; an invalid name or id is `400`. Every asset of §7.5 is served with its content type and the bytes of the vendored file, whose SHA-256 matches `SHA256SUMS`; any other asset path is `404`. No served page, `mimic.js` or `mimic.css` contains an `http://` or `https://` URL; `mimic.js` contains no `innerHTML`, `outerHTML`, `insertAdjacentHTML`, `document.write`, `eval` or `Function(`, and sets the Swagger UI options of §7.5 (`supportedSubmitMethods: []`, `tryItOutEnabled: false`, `validatorUrl: null`, `queryConfigEnabled: false`, the `requestInterceptor`). |
| UI-10 | **I.** Rendering bounds and metrics: with two renders held by a test hook, a third document request waits and completes once one is released, while `/v1/workloads` is answered meanwhile. After a known sequence of requests `mimic_ui_requests_total{route,code}` and the `mimic_ui_render_duration_seconds` count have the expected values; a request writes no stdout line; no line and no series contains a path, template, sample or report content. |
| UI-11 | **E.** On kind with the CTL-20 apps and traffic (a canary bearer token, a canary in a `PUT` body), through `kubectl port-forward` to a controller pod: `/v1/workloads/<ns>/users/openapi` lists `inbound-<users port>`; that document has `/users/{users_id}` with `get` and `put`, a `200` response with an example taken from a sample, the `http_bearer` scheme required by `get`, and `servers` naming the `users` Service host seen by the sidecar; `/endpoints` lists both endpoints with their counts. No canary is in any document, endpoint list or page. `HEAD /ui/` answers `200`. |
| UI-12 | **E.** Browser: the headless Chromium loads `/ui/`, the `users` deployment page and its document page through the port-forward and dumps each DOM: they list the deployment, its two endpoints and the operations `GET /users/{users_id}` and `PUT /users/{users_id}`; there is no "Try it out" or "Execute" button; Chromium's log shows no Content Security Policy violation and no request to an origin other than the port-forward. A body recorded with the key and value `<img src=/ui-xss onerror=alert(1)>` shows as text, with no `img` element of that `src` in the DOM; a body recorded as `{"$ref": "http://localhost:<canary port>/ref"}` makes no request reach a server listening on that port. The `users` pods log no access line during the browser run (the UI sends no traffic to workloads). |
| UI-13 | **C.** Supply chain: `scripts/vendor-swagger-ui.py --check` passes on the committed tree, fails when one vendored byte is changed and when the tarball does not match the lockfile's `sha512` (offline test with a fixture tarball); it runs in the supply-chain stage and `ci-ok` requires it. `scripts/check-cooldown.py` refuses a `swagger-ui-dist` version that a pull request adds or bumps in `crates/mimic-controller/ui/package-lock.json` when it was published less than 14 days before (`--now`), accepts it when the allowlist names it, and fails closed when the npm registry cannot be read. `.github/dependabot.yml` has an `npm` entry for `crates/mimic-controller/ui` with a 14-day cooldown; `zizmor` passes. |

### 12.8 F7 — Egress recording in the sidecar

Levels as in §12.2. Unit tests of §6.1 run on the recorder and the merge with the generators of
the DSC tests, extended to outbound exchanges (any authority, SNI and destination). Sidecar
integration tests run the real binary with the fake original destination of F2 on the
outbound listener, the interception harness of F3 (test root, fake sidecar API, a test HTTPS
destination) for TLS, and read `GET /discovery` from loopback. A *canary* is as in §12.5.

| ID | Criterion |
|---|---|
| EGR-01 | **U.** Scope key (§6.1): an absolute-form `http://API.Example.com./v1` gives `api.example.com:80`; `Host: users.disc-22.svc:8080` gives `users.disc-22.svc:8080`; `Host: api.example.com` on an intercepted connection gives `api.example.com:443`; an `h2` `:authority` wins over `Host`; `Host: [2001:DB8::1]:8443` gives `[2001:db8::1]:8443`; a missing or `(invalid)` authority gives the SNI with the destination port, else the destination `10.0.0.5:8080`; a host that a value detector catches gives `<redacted>:<port>`; every key produced matches the grammar of §6.1 (`proptest` on arbitrary `Host`, SNI and destination: no panic, never a control character, a space or a key over 261 bytes). |
| EGR-02 | **U.** Outbound endpoints: `GET /v1/payments/42` to `a.example:443` and to `b.example:443` are two endpoints `GET /v1/payments/{payments_id}`, and literal absorption at one authority leaves the other untouched; an outbound endpoint has no `hosts`; its scope's `origin` counts `schemes` `http` and `https`, and keeps at most 16 `sni` and 16 `dst`, the first in byte order; an outbound `404` on an unknown path creates its endpoint with a sample and a `405` is a sample candidate, while an inbound `404` is still `not_found` (DSC-11 unchanged). |
| EGR-03 | **U.** Format: a report without outbound scope serialises byte-identical to F5 (`"version": 1`, the DSC-09 and CTL fixtures unchanged); one with an outbound exchange is `"version": 2` with `scopes.outbound.<authority>` holding the tree, `origin` and `tunnels`; both round-trip through `from_json`; `from_json` rejects a version 1 report with an outbound scope, an inbound key that is not a port, an outbound key outside the grammar and a version 3; the push check of §7.3 still rejects a version 2 report (reason `version`). (Amended by SHR-01: from F8 the push check accepts version 2 with outbound.) |
| EGR-04 | **U.** Merge algebra with outbound (`proptest`, ≥ 10 000 cases): commutative and associative; recording a sequence of inbound and outbound exchanges one by one equals merging the reports of any partition of it, `origin` and `tunnels` included; past `max_authorities` the merge keeps the authorities that sort first, commutatively; `max_endpoints` is enforced per direction. |
| EGR-05 | **U.** Bounds: with `max_authorities` = 2, an exchange and a tunnel to a third authority are `authority_limit` while the two known ones still update; with `max_endpoints` = 3, three outbound endpoints do not refuse a new inbound one, nor the reverse; outbound samples stop at half of the sample budget (`budget`) while inbound samples are still admitted up to the whole budget. For arbitrary inbound and outbound exchanges (`proptest`) the recorder never panics and its retained state, sample bytes excluded, stays below the DSC-11 bound computed for both directions and the authority limit. |
| EGR-06 | **U.** Outbound view of redaction: in an outbound request, `Ocp-Apim-Subscription-Key: <canary>`, `X-Tenant-Key: <canary>` and `X-Anything: <canary>` keep their names and get the marker (rule `outbound_header`, fields marked `redacted`), while `accept`, `content-type`, `user-agent` and `if-none-match` values are kept, `Authorization: Basic <canary>` gives `Basic <redacted>` and `Cookie: a=<canary>` gives `a=<redacted>`; `?key=<canary>`, `?sig=<canary>` and a JSON `idempotency_key` are redacted by name; the same headers and keys in an outbound response and in an inbound request are kept (`X-Anything: v`, `?key=v`). An `AIza` key (`AIza` + 35 characters) is redacted by value in both directions, and `AIza` + 34 is kept. |
| EGR-07 | **U.** No leak (`proptest`, ≥ 10 000 cases): generated outbound exchanges carrying a canary under a listed name, in a detected form, or as the value of a request header with an arbitrary name outside the allowlist, at a random place (path, query, header, form or JSON body at any depth, request or response), give a serialised report that contains neither the canary nor its XXH3 hash, in decimal or hex. |
| EGR-08 | **U.** Tunnels: a `tcp`, a `connect` and each `tls_<reason>` kind of §6.1 are counted in the `tunnels` of their scope (SNI with the destination port, or the destination `ip:port`, or the `CONNECT` target), a scope can hold tunnels only (no endpoint), merges add the counts, and a tunnel creates no sample and no `hosts`. |
| EGR-09 | **I.** Cleartext outbound: HTTP/1.1 and `h2` (prior knowledge) exchanges from a test client through the outbound listener to the test destination (`GET /users/{1..5}` with JSON responses, `POST /orders?dry_run=true` with a JSON body answered `201` and `400`, a `GET /missing` answered `404`, a unary gRPC call) appear in `/discovery` under `scopes.outbound.<authority>` with their templates, status counters, histogram, query, header and body schemas, samples and `origin` (`schemes.http`, `dst`), the `404` included; `unframed` exchanges, exchanges without a response head and the sidecar's own probe and sidecar API requests do not; inbound exchanges sent meanwhile are recorded under `inbound` as in DSC-12; bytes reach both ends unchanged. |
| EGR-10 | **I.** Intercepted outbound: HTTPS requests over HTTP/1.1 and over `h2` (ALPN) to the test HTTPS destination are recorded with their decrypted heads and bodies under `<sni>:443`-style authorities, with `schemes.https` and the SNI in `origin.sni`; a `Host` that differs from the SNI keys the endpoint by `Host` and keeps the SNI in `origin.sni`; a `gzip` response body is decoded and inferred. |
| EGR-11 | **I.** Unrecordable traffic: a destination that asks for a client certificate (mTLS), a destination signed by an unknown CA, an excluded SNI, a client that rejects the mimic certificate, a sidecar without CA, an opaque TCP connection and a `CONNECT` answered `200` record no endpoint and count one `tunnels` entry each, of kinds `tls_client_cert_requested`, `tls_upstream_untrusted`, `tls_excluded`, `tls_client_rejected`, `tls_no_ca`, `tcp` and `connect`, under the scope of §6.1; `mimic_discovery_tunnels_total{kind,result="recorded"}` matches; the bytes of every case are unchanged. |
| EGR-12 | **I.** Latency: a destination that sends its response head 300 ms after the request gives outbound histogram observations at 300 ms or more, in the bucket of the `head_ms` of the matching access line; a destination whose TLS handshake is delayed by 500 ms and that answers at once gives a first exchange whose recorded `head_ms` is under 500 ms (connection setup is not in it). |
| EGR-13 | **I.** F7 boundary: with the fake sidecar API, every pushed report is `"version": 1` and holds no outbound scope while outbound and inbound traffic is sent; after an acknowledged push `/discovery` still shows the outbound endpoints with their counters unchanged (not reset) and only the unshipped inbound exchanges; `mimic_discovery_endpoints{direction="outbound"}` and `mimic_discovery_sample_bytes` include the outbound part. (Replaced by SHR-15 from F8: pushes carry the outbound part, which is reset after acknowledgement; the gauge clause still holds.) |
| EGR-14 | **I.** Shedding: with `MIMIC_DISCOVERY_QUEUE=1` and `MIMIC_DISCOVERY_CAPTURE_BUDGET=1Mi`, 2000 concurrent outbound requests with 200 KiB bodies (1800 cleartext, 200 intercepted) and 200 inbound ones sent at the same time are all answered with the bytes the destination or the app sent; `mimic_discovery_exchanges_total` sums, per direction, to the exchanges of that direction, with `dropped` > 0 for outbound, at least one inbound exchange `recorded`, and bodies skipped for the budget counted `budget`. |
| EGR-15 | **I.** Memory and limits: 256 inbound and 256 outbound endpoints over 64 authorities, each receiving 30 distinct 256 KiB JSON exchanges, leave the sidecar RSS under 128 MiB (the injected memory limit), with `mimic_discovery_sample_bytes` ≤ `MIMIC_DISCOVERY_SAMPLE_BUDGET` and the outbound samples at most half of it; a 65th authority is `authority_limit` with one WARN `authority limit reached`; a 257th outbound endpoint is `endpoint_limit` with one WARN `endpoint limit reached` (`direction` `outbound`) while a new inbound endpoint is still recorded. |
| EGR-16 | **I.** Logs and metrics: one INFO `endpoint discovered` per new outbound endpoint with `direction` `outbound`, `authority`, `method` and `template` and no `port`; `template absorbed` likewise; WARN `sample budget full` with `direction` `outbound` once when the outbound half fills; `mimic_discovery_exchanges_total{direction="outbound"}`, `mimic_discovery_bodies_total`, `mimic_discovery_samples_total`, `mimic_discovery_redactions_total{rule="outbound_header"}` and `mimic_discovery_authorities` match the traffic sent; no `mimic_discovery_*` series has an authority label; no line holds a header value, query, body or sample. |
| EGR-17 | **I.** No leak end to end: canaries sent by the client in outbound `Authorization`, `Cookie`, `X-Api-Key`, `Ocp-Apim-Subscription-Key` and `X-Custom` headers, a `?key=` and a `?sig=` query, an `AIza` key, a JWT path segment, `client_secret` in a form body to a token endpoint, and returned by the destination as `access_token` in a JSON body and in a `Set-Cookie`, cleartext and intercepted, appear neither in `/discovery`, nor on stdout (access lines included), nor in `/metrics`; the bytes reach the destination and the client unchanged. |
| EGR-18 | **I.** `MIMIC_DISCOVERY_MAX_AUTHORITIES` at `0`, `10001` or `abc` exits 2 before listening (FND-01); unset, the limit is 64. |
| EGR-19 | **E.** On kind, service A (`front`, `scripts/e2e/apps`) runs injected in its own `enabled` namespace with `UPSTREAM` the `users` Service of DSC-22 (service B); a client calls A's `/front/users/<id>` (GET with a canary bearer token and an `X-Tenant-Key` canary, PUT with a JSON body, some with B's `delay_ms=200`) and `/front/missing`. Through `kubectl port-forward`, A's `/discovery` holds `scopes.outbound."users.disc-22.svc:8080"` with `GET` and `PUT /users/{users_id}` (their counts equal to the client's calls), a `GET /missing` with a `404`, the delayed calls in histogram buckets of 200 ms or more, `Authorization: Bearer <redacted>` and `X-Tenant-Key: <redacted>` in samples (no `spec.discovery.redact` applies to `front`: the outbound allowlist does it), and none of the three canaries (the bearer token, the `X-Tenant-Key` value and the `password` of the PUT body) appears in `scopes.outbound` of `/discovery`, in A's stdout and `/metrics`, or in B's logs; neither the bearer token nor the password appears in A's inbound part, in the controller's merged report, nor in the Valkey values (the inbound `X-Tenant-Key` value of an unlisted header is the §11.1 limit, which `spec.discovery.redact` closes); B's access lines show every call (B got the traffic); the controller's merged report of `front` holds inbound endpoints only (F8). (Amended by SHR-19: from F8 the merged report of `front` holds the outbound endpoints, and A's `/discovery` holds only what it has not shipped yet, so the endpoint, count and histogram checks read the merged report; `/discovery` is still checked for canaries.) |
| EGR-20 | **C.** The DSC-23 fuzz target also feeds outbound exchanges (arbitrary authority, SNI, destination and tunnel kind) to the recorder; same CI schedule as DSC-23. |

### 12.9 F8 — Egress shared and aggregated by the controller

Levels as in §12.2. Unit tests of §7.3 to §7.6 run on reports built by the recorder and the
bounded merge with the generators of the CTL and EGR tests (inbound and outbound exchanges, any
authority, SNI, destination and tunnel kind). Controller integration tests run against a real
Valkey and the fake API server as in §12.6; sidecar integration tests use the fake sidecar API
of F3, which can be set to answer as a controller up to F7 (`400` with an empty body to any
report that is not `"version": 1`). A *canary* is as in §12.5. From F8, SHR-01 replaces the
version clause of CTL-01 and the last clause of EGR-03, SHR-15 replaces EGR-13, and the last
clause of EGR-19 no longer holds (SHR-19 checks the merged outbound report); their tests are
changed in the step that changes the behaviour. SHR-21 to SHR-23 come from enhancement #339
(the in-memory report of the controller): they change no output, only its memory cost.

| ID | Criterion |
|---|---|
| SHR-01 | **U.** Structural check with outbound (§7.3): it accepts every report the recorder produces with inbound and outbound exchanges and tunnels within the §6 and §6.1 bounds, merged or not (`proptest`, ≥ 10 000 cases), and a version 1 report exactly as CTL-01 does. It rejects, each with its fixed `reason`: a version 2 report without an outbound scope, a version 1 report with one, and a version 3 (`version`); an outbound key outside the §6.1 grammar (`A.example:80`, `a.example:080`, `a.example:0`, `a.example`, a 254-byte host) (`authority`); 17 `sni` or 17 `dst`, an SNI with an upper-case letter or over 253 bytes, a `dst` that is not an IP literal and port, a scheme `ftp` (`origin`); a tunnel kind `udp` (`tunnels`); `hosts` on an outbound endpoint (`hosts`); `origin` or `tunnels` on an inbound scope (`malformed`); `MIMIC_STORE_MAX_PUSH_ENDPOINTS` + 1 outbound endpoints and `MIMIC_STORE_MAX_AUTHORITIES` + 1 outbound scopes (tunnels only) are `push_limit`, while the same number of inbound endpoints alongside is not counted against the outbound limit. |
| SHR-02 | **U.** Re-redaction with the outbound view (§7.3): for outbound exchanges recorded by a redactor without the outbound view (an F7-era or faulty sidecar), with a canary at a random place (`proptest`, ≥ 10 000 cases): the value of a request header with an arbitrary name outside the allowlist, `?key=`, `?sig=`, a JSON `idempotency_key`, an `AIza` key, a JWT or token prefix in an allowlisted header (`user-agent`, `if-none-match`), in a header name, as the innermost JSON key at depth 4 or more, the host of an authority key and an `origin.sni` value in a detected form: the re-redacted report contains neither the canary nor its XXH3 hash in decimal or hex, redacted header values carry rule `outbound_header` and are marked `redacted`, an outbound response and an inbound request keep `X-Anything: v` and `?key=v`. A caught authority host becomes `<redacted>:<port>` and merges with an existing `<redacted>:<port>` scope (trees, `origin` and `tunnels` added); a caught SNI is removed. Re-redaction is idempotent and leaves a report recorded by an F8 sidecar with the same rules unchanged. |
| SHR-03 | **U.** Bounded merge with outbound (§7.3): below the limits it equals the §6 merge and does not depend on the order or grouping of pushes, `origin` and `tunnels` included (`proptest`). Anti-crowding of authorities with `MIMIC_STORE_MAX_AUTHORITIES` = 2: a store of 2 authorities with 100 exchanges each, merged with a push of 50 authorities of 1 exchange each, keeps the 2 (50 `authority_limit` refusals, no eviction, the refused authorities' endpoints and samples counted nowhere else); a push of an authority with 101 exchanges, or with 101 tunnel counts and no endpoint, evicts the stored authority with the least traffic, ties broken by key (`evicted` kind `authority` = 1); a new authority tying the lowest stored traffic does not displace it. With `MIMIC_STORE_MAX_ENDPOINTS` = 3, three outbound endpoints refuse no inbound one, nor the reverse. The same input always gives byte-identical output. |
| SHR-04 | **U.** Per-direction sample floor (§7.3), with a full budget `B`: while inbound holds at most `B`/2, a new outbound sample of a new (endpoint, status class) evicts outbound samples only, and is refused (`budget`) with nothing evicted when no outbound pair can give one; the same with the directions swapped; while inbound holds more than `B`/2, such an outbound sample evicts from the inbound pair holding the most samples. A stored report over a lowered budget is trimmed from the direction above half. For arbitrary sequences of pushes (`proptest`), a direction that holds at most `B`/2 before a merge holds no fewer samples after it, except those of its own endpoints evicted by its own new samples, and sample bytes never exceed `B` after a merge. |
| SHR-05 | **U.** Stamping, retention and the split of §7.3: stamping day `D` sets `last_seen = D` on every outbound scope, endpoint and sample of a push; dropping with day `T` and a TTL of `n` days removes an outbound scope whose `last_seen < T − n` once it holds no endpoint, and keeps one that still holds an endpoint seen later. Splitting a merged report gives an inbound part that is `"version": 1`, byte-identical to the report the same pushes without their outbound scopes would have stored (F7), and an outbound part that is `"version": 2` with outbound scopes only, or none; the union of the two parts is the merged report; each part passes the structural check. |
| SHR-06 | **U.** Status (§7.4): `:meta` with 2 inbound and 3 outbound endpoints gives `endpoints: {inbound: 2, outbound: 3}` and `totalEndpoints: 5`; without outbound, `outbound` is omitted and `totalEndpoints` equals `inbound`; without data `totalEndpoints` is `0`; byte-identical output for the same input; a status equal but for the defaulted `endpoints.inbound` and `totalEndpoints` compares equal. |
| SHR-07 | **U.** Outbound documents (§7.6): a report with scopes `api.example.com:443`, `10.0.0.5:8080`, `[2001:db8::1]:8443`, `<redacted>:443` and a tunnels-only `pay.example:443` gives documents with ids `outbound-<authority>` of each, alongside unchanged `inbound-<port>` documents (byte-identical to those of the same report without outbound); `info.title` is `<ns>/<dep> outbound`, `info.x-mimic-authority` the authority; `servers` is `http://…` and `https://…` per `origin.schemes`, port written, none for the marker; `x-mimic-origin` and `x-mimic-tunnels` as stored; the tunnels-only document has empty `paths`; `/v1/payments/{payments_id}` on two authorities is in two documents; outbound `Bearer <redacted>` gives `http_bearer`. The UI-01 determinism and OpenAPI 3.1 validity and the UI-06 no-Markdown property hold for reports with outbound scopes, with canaries also in SNI values, authority hosts and tunnel scopes (`proptest`). |
| SHR-08 | **U.** Index and endpoint list (§7.5): the index has one entry per outbound document with `direction` `outbound`, `authority`, `endpoints` and `tunnels`, sorted by id with the inbound ones; the endpoint list has the inbound entries first, unchanged, then the outbound ones by authority, template and method, with `authority`, no `port` and no `hosts`, each naming its document. The id grammar accepts every key the §6.1 grammar accepts and only those (`proptest` against the scope-key parser), and rejects `outbound-`, `outbound-a.example`, `outbound-A.example:80`, `outbound-a.example:080`, `outbound-a/b:80` and `inbound-80` written as `outbound-80`. |
| SHR-09 | **I.** Version 2 pushes (§7.2, §7.3): a push with outbound scopes answers `200` `{"applied": true}`; `:report` then holds the inbound part (`"version": 1`, byte-identical to the stored report of the same pushes without outbound) and `:outbound` the outbound part, each equal to the split of the bounded merge; `:meta` `endpoints_outbound` counts the outbound endpoints and `report_bytes` both keys; a deployment pushed only version 1 reports has no `:outbound` key and the keys and bytes of CTL-10; duplicates are `applied: false` with both keys unchanged; each SHR-01 violation is `400` with its `reason` in the `sidecar_api` line (with `version`), `MIMIC_STORE_MAX_AUTHORITIES` + 1 outbound scopes (= 2 in the test) is `507` `push_limit` with nothing stored; the CTL-09 replica test passes with outbound pushes. With `MIMIC_STORE_MAX_AUTHORITIES` = 2 across pushes, `mimic_store_refused_total{kind="authority",reason="authority_limit"}` and `mimic_store_evicted_total{kind="authority"}` follow, with one WARN `store limit reached` (`reason` `authority_limit`) and one WARN `store evicted` (`kind` `authority`) per deployment; a corrupted `:outbound` gives ERROR `stored report unreadable` with `key` `outbound`, push `503`, read `500`, and is not overwritten. |
| SHR-10 | **I.** No leak: version 2 pushes carrying the EGR-17 canaries unredacted in their outbound part (as a faulty sidecar would send them: headers outside the allowlist, `?key=`, `?sig=`, an `AIza` key, a JWT in an allowlisted header, a detected form as an authority host and as an SNI) and a canary under a name only `spec.discovery.redact` lists are applied, and no value of any `mimic:v1:*` key (`SCAN` dump), no line of stdout, no `/metrics` series, and no answer of the `report`, `endpoints`, `openapi` and document routes contains a canary, its XXH3 hash or the token. |
| SHR-11 | **I.** `MIMIC_STORE_MAX_AUTHORITIES` at `0`, `10001` or `abc` exits 2 before listening (FND-01); unset, the limit is 128. |
| SHR-12 | **I.** Status: within `MIMIC_STATUS_INTERVAL` (+ 1 s) of a version 2 push, the `MimicWorkload` gets `endpoints.outbound`, `totalEndpoints` and `exchanges` of both directions through the status subresource (CTL-18 rules: server-side apply, no request when unchanged). |
| SHR-13 | **I.** Read routes with outbound: `/v1/workloads/<ns>/<dep>/report` answers the union of `:report` and `:outbound` (`"version": 2`), and the `:report` bytes when there is no `:outbound`; `/openapi` lists the outbound documents; `/openapi/outbound-users.disc-22.svc:8080` and `/openapi/outbound-%5B2001%3Adb8%3A%3A1%5D%3A8443` answer `200` with exactly the §7.6 output; `outbound-%3Credacted%3E%3A443` is served when stored; `400` for `outbound-a%2Fb:80`, `outbound-%ZZ`, a segment that decodes to invalid UTF-8 and the SHR-08 invalid ids, `404` for an absent authority; the page `/ui/workloads/<ns>/<dep>/openapi/<encoded id>` is served; `mimic.js` builds document links with `encodeURIComponent`; UI-07 and UI-08 hold on the outbound routes. |
| SHR-14 | **I.** Compatibility of a version 1 sidecar: pushes of the F7 fixtures (version 1, recorded by an F7 sidecar) are applied, stored and rendered byte-identically to F7, and a controller started on a Valkey written by F7 serves and merges its data unchanged. A controller up to F7 started on a Valkey written by F8 (the `:report` bytes checked with the F7 rule: version 1, no outbound scope) reads and merges every deployment. |
| SHR-15 | **I.** Shipping outbound (§6.1, §7.2): with the fake sidecar API accepting version 2 and outbound, inbound and tunnel traffic, every push that holds an outbound scope is `"version": 2`; per direction, the exchanges of all pushes sum to the exchanges recorded, and the tunnel counts to the tunnels recorded; an interval with only a tunnel is pushed; after an acknowledged push `/discovery` holds only outbound exchanges recorded since (counters, samples, `origin` and `tunnels` reset); outbound samples of the pending and current reports together stay at most half of `MIMIC_DISCOVERY_SAMPLE_BUDGET`; without `MIMIC_CONTROLLER_URL` the outbound part accumulates as in F7. |
| SHR-16 | **I.** Fallback to inbound only (§7.2): with the fake sidecar API set as a controller up to F7, a pending version 2 report is followed by a post of the same session and sequence whose report is `"version": 1`, inbound only, and is applied; the fake API receives every inbound exchange once and no outbound scope; `/discovery` keeps every outbound exchange recorded so far (counts accumulated, none lost); the next interval tries version 2 again; once the fake accepts version 2, the next push carries every outbound exchange recorded since the start, each once. A report with outbound and no inbound exchange posts nothing after the `400`, and the next freeze uses the same sequence. When the first attempt has no answer (the fake applies it, then closes the connection) and the retry is answered `400`, the inbound-only post follows, the outbound part is dropped (not in `/discovery`, `mimic_discovery_outbound_dropped_total` 1, one WARN `outbound not shipped` with `reason` `unknown_outcome`), and the outbound exchanges are counted once overall. A `507` to a version 2 report falls back the same way; a `400` to the inbound-only post drops it with one WARN `report rejected`. At `SIGTERM` the flush falls back the same way and ships the inbound part within `MIMIC_SHUTDOWN_GRACE`. `mimic_discovery_pushes_total{result="inbound_only"}` counts the fallbacks; one WARN `outbound not shipped` (`status`) per streak, written again after an acknowledged version 2 push ends a streak; no line holds the token or report content. |
| SHR-17 | **C.** The CTL-22 fuzz target also feeds version 2 reports (arbitrary outbound keys, `origin`, `tunnels`) to `Report::from_json`, the structural check, re-redaction, the bounded merge and the split: no panic, every merge result and both split parts pass the check. Same CI schedule as DSC-23. |
| SHR-18 | **I.** Controller memory: two concurrent pushes (the second waits for the body budget or gets `503`, and is re-posted) onto deployments whose stored reports are each at the store limits (512 inbound and 512 outbound endpoints over 128 authorities, schemas at their caps, `MIMIC_STORE_SAMPLE_BUDGET` full), with a body near `MIMIC_SIDECAR_API_MAX_REPORT` and the default `MIMIC_SIDECAR_API_BODY_BUDGET` (`32Mi`), then a document of each kind rendered per deployment, keep the controller's peak RSS (`VmHWM`) under 640 MiB (128 MiB of headroom under the chart's 768 Mi limit). |
| SHR-19 | **E.** On kind, `front` (service A, `scripts/e2e/apps`) runs with 2 replicas injected in its own `enabled` namespace with a `MimicWorkload`, `UPSTREAM` the `users` Service (service B); a client sends calls spread over both pods to `/front/users/<id>` (GET with a canary bearer token and an `X-Tenant-Key` canary, PUT with a JSON body holding a canary) and `/front/missing`. Within 2 push intervals, through `kubectl port-forward` to a controller pod: front's `/report` holds `scopes.outbound."users.disc-22.svc:8080"` with `GET` and `PUT /users/{users_id}` and `GET /missing` (`404`), whose counts are the sums of the outbound calls in the two front pods' access lines, each pod having sent some (the union of both pods); `/v1/workloads/<ns>/front` lists both pods; `/openapi` lists `outbound-users.disc-22.svc:8080` and its document has `/users/{users_id}` with `get` and `put`, `servers` `http://users.disc-22.svc:8080`, and `X-Tenant-Key` as a redacted header parameter; within `MIMIC_STATUS_INTERVAL` more the `MimicWorkload` of front shows `endpoints.outbound: 3` and `totalEndpoints` equal to its inbound plus 3, and `kubectl get mimicworkload` prints that total under `Endpoints`. No canary is in front's report, documents, any Valkey value, the controller's logs or the pods' `/discovery`. The `users` checks of CTL-20 and UI-11 still pass. |
| SHR-20 | **E.** Browser: the headless Chromium of UI-12 loads front's deployment page and its `outbound-users.disc-22.svc:8080` document page: the endpoint list shows the outbound endpoints with their authority, the document shows `GET /users/{users_id}`, there is no "Try it out" or "Execute" button, Chromium's log shows no CSP violation and no request to an origin other than the port-forward, and the `users` pods log no access line during the run. |
| SHR-21 | **U.** Byte identity of the compact report (#339): a corpus committed before the first change of #339 (`crates/mimic-core/tests/golden/`: reports built by the recorder and the bounded merge from fixed seeds of the CTL, EGR and SHR generators, inbound and outbound, at and below the store limits) is reproduced byte for byte: `to_json`, both split parts, the §7.6 documents, the §7.5 index and endpoint list, and the §7.7 egress projection and digest. `to_json(from_json(x))` equals `x` for every report the recorder and the bounded merge produce (`proptest`, ≥ 10 000 cases); `from_json` accepts object keys in any order and keeps the last of duplicate keys at every map of the report, as before. Merging a push read from its JSON into a stored report (§7.3, write) gives the same bytes and the same `refused` and `evicted` counts as the bounded merge of the parsed push, for any order and grouping of pushes (`proptest`, ≥ 10 000 cases). |
| SHR-22 | **I.** Footprint of a parsed report (`crates/mimic-core/tests`, `dhat` heap testing): the report of SHR-18 at the store limits (512 inbound and 512 outbound endpoints over 128 authorities, schemas at their caps, a full 4 MiB sample budget), read with `Report::from_json`, holds live heap bytes plus 16 bytes per live block of at most 1.6 times its JSON length (3.1 times before #339). |
| SHR-23 | **I.** Concurrent writes (#339): the SHR-18 scenario with `MIMIC_SIDECAR_API_BODY_BUDGET` at `64Mi` (both pushes are read and written at once: neither gets a `503`) keeps the controller's peak RSS (`VmHWM`) under 640 MiB, and the SHR-18 scenario with the defaults peaks (highest of 5 runs) at no more than 70% of B, the highest of 5 runs of the same test before the first memory change of #339 (B is 509 MiB, measured by step 1 of #339: 422, 422, 509, 423 and 508 MiB over 5 runs, so the bound is 356 MiB). In both, the test holds each push body once and re-posts it without a copy, so that the peak is the controller's. Measurement conditions (`crates/mimic-controller/src/sidecar_api/push_tests/memory.rs`): the controller code runs in a child process of the test binary with `MALLOC_ARENA_MAX=2` (as the chart), the stored reports are at the store limits before the measured pushes, and the peak is `VmHWM` after the pushes and one document of each kind per deployment. After step 5 of #339 the concurrent test peaks at 438 to 536 MiB (5 runs) and is asserted; the 70% bound peaked at 308 to 368 MiB over 10 runs. Step 6 of #339 (the staged, chunked write of §7.3: the peak was the two copies of the output that redis-rs makes at the end of a write, 74 MiB) brings the SHR-18 defaults to 219 to 272 MiB over 10 runs (asserted at 356 MiB) and the concurrent test to 296 to 383 MiB over 5 runs. |

### 12.10 F9 — Egress endpoint for sidecars

Levels as in §12.2. Unit tests build reports with the recorder and the bounded merge and the
generators of the CTL, EGR and SHR tests. Controller integration tests run against a real
Valkey and the fake API server as in §12.6; sidecar integration tests use the fake sidecar API
of F3, extended to serve an egress set (with `ETag`, `304` and any status or body) and to
answer `404` as a controller up to F8. A *canary* is as in §12.5. F9 changes no row of
earlier features: SHR-09 and SHR-18 must still pass with the digest write and the egress
route in place.

| ID | Criterion |
|---|---|
| PUL-01 | **U.** Projection (§7.7), for reports with inbound and outbound exchanges and tunnels built by the recorder and the bounded merge (`proptest`, ≥ 10 000 cases): one authority per outbound scope that holds an endpoint, tunnel-only scopes and the inbound part absent (the projection of the union equals that of its `:outbound` part); per endpoint `method`, `template`, `grpc`, `statuses` and `latency` equal to the report's, `samples` equal to the report's without `last_seen`, in report order, `volatile` and `redacted` equal to the `x-mimic-volatile` and `x-mimic-redacted` of the same endpoint's §7.6 document; no key `schema`, sketch, `origin`, `tunnels`, `hosts`, `last_seen` or `pod` at any depth; byte-identical output for equal input; a report without outbound scope gives `{"authorities":{},"format":1}`; `EgressSet::from_json` accepts every projection and serialises it back byte-identically. |
| PUL-02 | **U.** Egress set check (§7.7) refuses, each with its `reason`: `format` 0, 2 or absent (`format`); an authority `A.example:80`, `a.example:080` or `a.example` (`authority`); 10 001 authorities, 10 001 endpoints in all (`limit`); a template of 2049 bytes or 33 segments, a status 99 or 600, a histogram bucket outside the §6 range, 21 samples, a sample whose `bytes` is not its serialised size, 129 field names, a field name of 129 bytes, an unknown field at any level, a body that is not JSON (`malformed`). |
| PUL-03 | **U.** No leak in the projection: reports that held a canary at a random place (the SHR-02 generator, inbound and outbound) re-redacted by §7.3 then projected contain neither the canary nor its XXH3 hash in decimal or hex, and no hash of any volatility sketch of the source report (`proptest`, ≥ 10 000 cases). |
| PUL-04 | **U.** Digest and `ETag` (§7.7): the digest of version `v` and bytes `b` is `v:<hex SHA-256 of b>` (of the empty string without `:outbound`); the `ETag` is `"1-<hex>"`; a digest field that is absent, malformed or written for another `version` is stale; `If-None-Match` matches when one entry of its list equals the `ETag` exactly (`W/"…"` and `*` do not match). |
| PUL-05 | **U.** Local view (§7.7): the summary is a function of the snapshot, its `ETag`, the `pulled` time and (from F10) the mode and latency factor; it lists every authority and endpoint with `method`, `template`, `grpc`, `exchanges` (the sum of `statuses`) and `statuses`, sorted keys, byte-identical for equal inputs (snapshot, `ETag`, `pulled`, mode and factor); it holds no sample, no field name and none of the PUL-03 canaries; without snapshot in discovery with factor 1 it is `{"authorities":{},"etag":null,"latency_factor":1,"mode":"discovery","pulled":null}` (amended by F10: up to F9 it was `{"authorities":{},"etag":null,"pulled":null}`). |
| PUL-06 | **I.** Digest in the store (§7.3, §7.7): after a version 2 push `:meta` `outbound_digest` is `<version>:<SHA-256 of the :outbound bytes>`; after a version 1 push that leaves `:outbound` unchanged its hash part is unchanged and its version follows `version`; a deployment without `:outbound` holds the empty-string digest; `:report`, `:outbound` and the other `:meta` fields are byte-identical to what F8 writes for the same pushes. After an F8-style write (digest removed or left at an older version, `:outbound` changed), the route answers the projection of the stored bytes with their `ETag`, and an `If-None-Match` of the earlier `ETag` gets `200`. |
| PUL-07 | **I.** Route (§7.7): a token of a pod of the deployment gets `200` `application/json` with `ETag` and `Cache-Control: no-store`, the body byte-identical to the `mimic-core` projection of the stored `:outbound`; the same `If-None-Match` gets `304` with the `ETag`, an empty body and no `GET` of `:outbound` (Valkey `INFO commandstats`); after a version 2 push that changes the outbound part the next pull is `200` with a new `ETag`, after a version 1 push it is still `304`; a deployment without data and one with inbound data only get `200` `{"authorities":{},"format":1}`; two replicas answer the same `ETag` and bytes. |
| PUL-08 | **I.** Refusals (§7.7): missing, invalid and foreign-audience tokens get `401`; a pod of another deployment or without bound-pod claim `403`; a pod of namespace `b` naming a deployment that has data only in namespace `a` gets the empty set; over `MIMIC_SIDECAR_API_RATE` `429` with no `TokenReview`; `format` absent, `2` or `abc` `400` (`format`); a name that is not DNS-1123 `400`; `POST` and `PUT` `405` with `Allow: GET`; Valkey stopped `503` with `Retry-After: 1`; a corrupted `:outbound` `500` with one ERROR `stored report unreadable` (`key` `outbound`), the key left unchanged; a projection over `MIMIC_SIDECAR_API_MAX_EGRESS` (64Ki in the test) `507` (`egress_limit`) with one WARN `egress limit reached` (`reason` `projection`) per deployment. |
| PUL-09 | **I.** Builds and budgets (§7.7): 32 concurrent pulls of one deployment at a new digest make one build (`mimic_sidecar_api_egress_builds_total{result="ok"}` + 1) and 32 identical `200` answers; with `MIMIC_SIDECAR_API_EGRESS_BUDGET` twice a projection, pulls of three deployments in turn keep `mimic_sidecar_api_egress_cache_bytes` at most the budget and the least recently served is built again at its next pull; a projection that cannot fit while others are being sent gets `503` with `Retry-After: 1`; while pushes hold the whole `MIMIC_SIDECAR_API_BODY_BUDGET`, a pull that needs a build gets `503` and a pull answered from the cache or by `304` does not; a stale digest (PUL-06) takes the same share (it gets `503` too while the pushes hold the budget); an `:outbound` whose `STRLEN` plus `MIMIC_SIDECAR_API_MAX_EGRESS` exceeds the whole body budget (256Ki budget in the test) gets `507` (`egress_limit`) at every pull, never `503`, without a `GET` of `:outbound`, with one WARN `egress limit reached` (`reason` `stored`) and `mimic_sidecar_api_egress_builds_total{result="too_large"}` counting each; two deployments of the same name in two namespaces never get each other's projection from the cache. |
| PUL-10 | **I.** Logs and metrics of the route: one `sidecar_api` line per pull with `endpoint` `/v1/workloads/{deployment}/egress`, `namespace`, `pod`, `deployment`, `result` (`ok`, `not_modified`, `unauthenticated`, `forbidden`, `invalid` with `reason` `format`, `egress_limit`, `unavailable`; `rate_limited` at DEBUG) and `bytes`; `mimic_sidecar_api_requests_total{endpoint="/v1/workloads/{deployment}/egress",result}` follows. With the SHR-10 canary pushes applied, no answer of the route, no line of stdout and no `/metrics` series holds a canary, its XXH3 hash or the token, and no series holds a template or an authority. |
| PUL-11 | **I.** Controller configuration: `MIMIC_SIDECAR_API_MAX_EGRESS` at `0`, `63Ki`, `2Gi` or `abc`, and `MIMIC_SIDECAR_API_EGRESS_BUDGET` below `MIMIC_SIDECAR_API_MAX_EGRESS` or over `4Gi`, exit 2 before listening (FND-01); unset, they are `4Mi` and `32Mi`. |
| PUL-12 | **I.** Controller memory: the SHR-18 scenario with default settings plus, during the two pushes, 32 concurrent pulls of each deployment at a new digest (each pull builds; a projection over `MIMIC_SIDECAR_API_MAX_EGRESS` is built, then refused), keeps the controller's peak RSS (`VmHWM`) under 672 MiB (96 MiB of headroom under the chart's 768 Mi limit); every pull gets `200`, `503` or `507`. The bound is 640 MiB (SHR-18) plus the 32 MiB egress budget: builds add nothing beyond the body budget SHR-18 already measures, since their share counts the stored bytes and the output buffer (`MIMIC_SIDECAR_API_MAX_EGRESS`) and displaces pushes. If it is missed, step 4 reports the measurement on #242 rather than raising the bound. |
| PUL-13 | **I.** Pull client (§7.7), against the fake sidecar API with `MIMIC_EGRESS_PULL_INTERVAL=2s`: the first `GET /v1/workloads/<MIMIC_WORKLOAD>/egress?format=1` arrives within one interval of start, without `If-None-Match`, with the token the file holds at that moment (the file changes between two pulls and the next pull carries the new one); later pulls come once per interval with the `ETag` held; `304` keeps the snapshot and moves `pulled`; a new `200` replaces it; an empty set empties it; `/egress`, `mimic_egress_endpoints`, `mimic_egress_bytes`, `mimic_egress_last_pull_timestamp_seconds` and `mimic_egress_pulls_total{result="ok"|"not_modified"}` follow. |
| PUL-14 | **I.** Pull failures keep the snapshot held (§7.7): `503`, a reset, an answer delayed past 30 s, `401`, `403` and `429` are `error` and retried after 1, 2, 4 … s (`Retry-After: 3` waits at least 3 s, `Retry-After: 900` waits 300 s); one WARN `egress pull failed` per streak and one INFO `egress pull recovered`; `404` and `405` are `unsupported` and the next pull comes one interval later, not sooner; a 1 GiB body is `too_large` and the sidecar stops reading at `MIMIC_EGRESS_MAX_BYTES` (its RSS grows by less than twice the cap); a body with `format` 2, one that is not JSON, and one with 21 samples in an endpoint are `invalid` with one WARN `egress rejected` carrying the PUL-02 `reason`; `507` is `too_large`. No line holds the token or snapshot content. |
| PUL-15 | **I.** F9 changes no traffic: with a snapshot holding the authority and endpoint the app calls, the outbound calls still reach the destination and the app gets its answers unchanged (no `x-mimic-match` header); access lines, `/discovery` and the pushed reports are the same as with an empty snapshot (timings aside). Without `MIMIC_CONTROLLER_URL` nothing is pulled and one INFO `egress pull disabled` is written. At `SIGTERM` no pull starts after the signal, and a pull in flight to a silent fake does not delay the exit beyond the FND-05 bound. |
| PUL-16 | **I.** `/egress` on the sidecar admin port: a non-loopback peer gets `403`; a loopback peer gets the PUL-05 summary of the snapshot held and (from F10) of the current mode and latency factor; with a snapshot whose samples hold canaries, no canary is in `/egress`, the sidecar's stdout or `/metrics`. |
| PUL-17 | **I.** Sidecar configuration: `MIMIC_EGRESS_PULL_INTERVAL` at `0s`, `2h` or `abc`, and `MIMIC_EGRESS_MAX_BYTES` at `63Ki`, `2Gi` or `abc`, exit 2 before listening (FND-01); unset, they are `60s` and `4Mi`. |
| PUL-18 | **I.** Sidecar memory, combined with discovery: with default settings, while the EGR-15 load runs (256 inbound and 256 outbound endpoints over 64 authorities, each receiving 30 distinct 256 KiB JSON exchanges, the worst case of DSC-17 included), the sidecar pulls from the fake sidecar API two different egress sets of just under `MIMIC_EGRESS_MAX_BYTES` each (samples with JSON bodies of many small keys, the costliest to parse), one after the other with the first still held while the second parses, and a third pull returns a refused body of the same size; its peak RSS (`VmHWM`) stays under 120 MiB (8 MiB of headroom under the injected 128 Mi limit), and after each accepted pull `/egress` shows the new set. |
| PUL-19 | **E.** On kind, after the traffic of SHR-19 stops, within 2 push intervals plus 2 pull intervals: through `kubectl port-forward` to each `front` pod's admin port, `/egress` lists `users.disc-22.svc:8080` with `GET` and `PUT /users/{users_id}` and `GET /missing`, with the same `etag` on both pods; a `users` pod's `/egress` has no authority; the controllers' logs hold `sidecar_api` lines for `/v1/workloads/{deployment}/egress` with `ok` and `not_modified`; no SHR-19 canary is in any `/egress` answer or controller log line. |
| PUL-20 | **C.** A fuzz target feeds arbitrary bytes to `EgressSet::from_json`, and the reports of the CTL-22 corpus that pass the §7.3 check to the projection: no panic, and every projection passes the egress set check. Same CI schedule as DSC-23. |

### 12.11 F10 — Mock mode, and back to discovery

Levels as in §12.2. Unit tests build egress sets with the recorder, the bounded merge and the
projection of F9 (so that samples, `volatile` and `redacted` are what production gives).
Sidecar integration tests use the fake sidecar API of F9, extended to send `Mimic-Mode` and
`Mimic-Latency-Factor` (or none) and to change them between pulls, and fake destinations
that count accepted connections and received bytes; "no egress" means that count is 0.
Controller integration tests run against a real Valkey and the fake API server as in §12.6. A
*canary* is as in §12.5. F10 amends PUL-05 and PUL-16 in place (the `/egress` summary gains
`mode` and `latency_factor`); every other earlier row must still pass, PUL-15 included with
`MIMIC_MODE=discovery` and answers carrying `Mimic-Mode: discovery`.

| ID | Criterion |
|---|---|
| MCK-01 | **U.** Effective mode (§7): no `MimicWorkload` gives `discovery`, factor 1, no passthrough; `discovery` + `mock` gives `mock`; the factor is that of the first `mock` one in name order; `spec.mock.passthrough` is the sorted, deduplicated union, cut to its first 256. The injection patch always sets `MIMIC_MODE` to the effective mode, sets `MIMIC_MOCK_LATENCY_FACTOR` only when the factor is not 1 and `MIMIC_MOCK_PASSTHROUGH` only when the union is not empty. **E.** The CRD defaults `spec.mock.latencyFactor` to 1 when `spec.mock` is given, accepts 0, 0.5 and 100, rejects -1, 101 and a string, and rejects a `spec.mock.passthrough` entry `*` and 257 entries. |
| MCK-02 | **U.** Endpoint lookup (§8): an authority absent from the set is a miss `authority` (an empty set too); `GET /users/42` takes `GET /users/{users_id}`; `GET /users/me` takes `GET /users/me` before `GET /users/{users_id}` whatever their order in the set; `HEAD /users/42`, `POST /users/42` and `GET /users/42/x` are misses `endpoint`; a gRPC request on `/pkg.Svc/Get` takes only the `grpc` endpoint of that exact path; `Host: API.Example.COM.` on a cleartext connection to port 80 looks up `api.example.com:80`, and without `Host` on an intercepted connection the SNI with the destination port; a path segment holding a JWT matches the template whose segment at that position is a parameter. |
| MCK-03 | **U.** Tiers (§8), on sets built by the recorder: a request equal to a sample but for a volatile query key, a redacted JSON field, a field whose sample value is `<redacted>` and any header value is `exact` with that sample; a request differing in one non-volatile body leaf is `endpoint` with the sample sharing the most fields, ties going to the first in set order; a request sharing no field is `fallback` with the first sample of the most frequent status (statuses `{200: 5, 404: 5}` give `200`); an endpoint whose most frequent status has no sample answers that status with an empty body; a `101` sample or status is never used, and an endpoint holding only `101` is a miss `endpoint`. The result is the same for the same set and request (deterministic) and never depends on the order of the request's query keys of different names. |
| MCK-04 | **U.** Answer (§8): the sample's status and headers in order without `content-length`, `content-encoding`, `transfer-encoding`, `connection`, `keep-alive`, `te`, `trailer`, `upgrade` and `proxy-connection`, plus `content-length` equal to the body length and `x-mimic-match`; a JSON body serialised compactly reads back to the sample's value; a form body encodes its pairs in order; a sample without body, and `HEAD`, `204` and `304`, give an empty body; a gRPC answer is status 200 with the sample's `content-type`, the body `00 00 00 00 00` and trailers `grpc-status: 0`; a miss is `599` with `x-mimic-miss: 1`, `content-length: 0` and nothing else. Property (`proptest`, ≥ 10 000 cases, any set and request): every header and body value of an answer comes from one sample of the request's authority and endpoint in that set or from the fixed values above, and a canary (a random string of 32 characters of `A-Za-z0-9`, drawn per case) placed in the request's headers, query or body is not in the answer. |
| MCK-05 | **U.** Latency draw (§8), with the inputs the caller supplies: a draw is a whole number of milliseconds in the chosen bucket (bucket 0: 0 or 1); for each bucket, the inputs that select it are a share of the input range equal to its count over the total (exact, by enumeration on a small histogram); the overflow bucket gives 131 072 ms; an empty histogram gives 0; factor 0 gives 0, factor 2 twice the factor-1 draw for the same inputs, and no draw exceeds 300 s. Rounding: a draw `d` with factor `f` hundredths gives `(d × f + 50) / 100` ms: factor 0.5 on 3 ms gives 2 ms, 1.3 on 10 ms 13 ms, 0.01 on 49 ms 0 ms and on 50 ms 1 ms; a factor written `0.005` reads as 0.01, `0.004` as 0. |
| MCK-06 | **U.** Request signature (§8): a request with `?api_key=<canary>`, `Authorization: Bearer <canary>`, a JSON body `{"token": "<canary>", "amount": 3}` and a card number in a path segment is matched as the recorder would have recorded it: it is `exact` with the sample the recorder made of the same exchange, and none of the fields compared holds the canary. Mode headers: `Mimic-Mode` `discovery` and `mock` are read, anything else (case changed, empty, two values) is invalid; `Mimic-Latency-Factor` reads decimals 0–100, and the controller writes `1`, `0.5`, `0` and `100` in that form. |
| MCK-07 | **U.** Mode convergence (§7.4): from `:pulls` fields, a window and a time, `status.mode.pods` counts the fields within the window carrying the effective mode and `otherPods` those carrying the other one; fields outside the window or unreadable are in neither count. |
| MCK-08 | **I** (controller). Mode on the egress route (§7.7): with one `MimicWorkload` in `mock` (factor 0.5) for the deployment, `200` and `304` answers carry `Mimic-Mode: mock` and `Mimic-Latency-Factor: 0.5`; after `spec.mode` is set to `discovery` in the fake API server, the next pull with the same `If-None-Match` is still `304`, with the same `ETag`, and carries `Mimic-Mode: discovery` and no factor; a second `MimicWorkload` in `mock` naming the same deployment turns it back to `mock`; a pod of namespace `b` gets the mode of its own deployment, never that of `a`; `400`, `401`, `403`, `429`, `503` and `507` answers carry no mode header. After each `200`/`304`, `:pulls` holds `<pod>` → `<mode> <ms>` with the expiry of `:pods` fields; with the `:pulls` write failing the pull is still answered, and `mimic_store_writes_total{result="error"}` counts it; with every `:pulls` write stalled, 40 pulls are answered within 1 s and `mimic_store_writes_total{result="dropped"}` counts those past 16 in flight. |
| MCK-09 | **I** (controller). Status: with `MIMIC_STATUS_POD_WINDOW=10s`, two pods pulling in `mock` and one whose last pull carried `discovery` give `status.mode` `{mode: mock, pods: 2, otherPods: 1}`; 10 s (+ one status interval) after that pod's last pull, `otherPods` is 0. **E.** `kubectl get mimicworkload` prints a `Mode pods` column. |
| MCK-10 | **I.** Sidecar mode state (§3, §7.7, §8): with `MIMIC_MODE=mock` and `MIMIC_EGRESS_PULL_INTERVAL=60s` the first pull arrives within 1 s of start; `mode changed` lines (`source` `env` at start, then `pull` at each change of mode or factor) and `mimic_mock_mode` follow the answers; an answer without `Mimic-Mode` or with `Mimic-Mode: MOCK` keeps the current mode with one WARN `mode unknown` per streak; a `200` refused as `too_large` or `invalid` still applies its mode; `503`, a reset and `404` keep the mode; `/egress` shows `mode` and `latency_factor`. State file: after a pull switched it to `mock` (factor 0.5), `/mimic/state/mode` holds `{"latency_factor":0.5,"mode":"mock"}`; a sidecar restarted with the same `/mimic` and `MIMIC_MODE=discovery`, its controller silent (no answer), answers outbound requests from the mock from its first one with `source` `state`, and the fake destination accepts no connection; an invalid file is ignored with one WARN `mode state ignored` and `MIMIC_MODE` applies; an unwritable `/mimic/state` gives one WARN `mode state not saved` and the switch still applies. `MIMIC_MODE=record`, `MIMIC_MOCK_LATENCY_FACTOR` at `-1`, `101` or `abc` and `MIMIC_MOCK_PASSTHROUGH=*` exit 2 before listening (FND-01); unset they are `discovery`, `1` and empty. |
| MCK-11 | **I.** No real egress, HTTP/1 (§8): in mock, with the set holding `GET /users/{users_id}` for `users.test:8080`, the app's request to `users.test:8080` gets the sample's answer with `x-mimic-match`, a request to an unknown path and one to an unknown authority get `599` with `x-mimic-miss: 1`, a `CONNECT` and a WebSocket upgrade get `599` (`connect`, `upgrade`), keep-alive and 5 pipelined requests are answered in order, `Expect: 100-continue` gets `100` then the answer; the fake destination accepts no connection in any of these cases, also when nothing listens there; access lines carry `mock` and `mock_miss`, `mimic_mock_requests_total`, `mimic_mock_miss_total` and `mimic_http_requests_total{direction="outbound"}` count each request, and `mimic_discovery_exchanges_total{direction="outbound"}` and the outbound part of `/discovery` do not change. |
| MCK-12 | **I.** HTTP/2 and gRPC (§8): in mock, 32 concurrent `h2c` streams to a recorded authority are answered on one connection with their samples; a gRPC unary call (length-prefixed message, `application/grpc`, trailers checked) to a recorded method succeeds with the default message and `grpc-status` 0; an unrecorded gRPC method fails at the client (HTTP status 599, not `OK`); the fake destination accepts no connection. |
| MCK-13 | **I.** TLS in mock (§5, §8): a client trusting `MIMIC_TLS_CA` and offering `h2,http/1.1` to SNI `api.test`, with nothing listening at the destination, gets a leaf for `api.test` from the intermediate, ALPN `h2` and the mocked answer; with `http/1.1` only, HTTP/1.1; a ClientHello without SNI to `127.0.0.1` gets a leaf with that IP SAN; an SNI matching `MIMIC_TLS_EXCLUDE`, a sidecar without CA and a TLS 1.1 ClientHello are closed with no connection at the destination, a `connection` line with `result: mock_closed` and its `interception`, and `mimic_mock_connections_total{decision="closed"}`; a decrypted stream that is not HTTP is closed the same way. |
| MCK-14 | **I.** Non-HTTP and passthrough (§8): in mock, a client-first non-HTTP stream (a PostgreSQL `SSLRequest`) reaches its destination unchanged and is recorded as a `tcp` tunnel; a server-first destination with a silent client is connected between 1 s and 1.5 s after accept and its greeting reaches the client; a client that sends an HTTP request after 1.2 s to a destination that stays silent gets the mocked answer, while the destination saw one connection, received 0 bytes and was closed; with `MIMIC_MOCK_PASSTHROUGH=users.test`, HTTP to `users.test:8080` and TLS with SNI `users.test` reach the destination as in discovery (PRX-03, TLS-17) and are recorded; `mimic_mock_connections_total{decision}` counts `mocked`, `tunnel`, `passthrough` and `closed`. |
| MCK-15 | **I.** Latency (§8): with a set whose endpoint histogram has all its exchanges in bucket 31 (182–215 ms), 20 mocked requests each get their response head between 182 ms and 215 ms (+ 50 ms) after their first byte, and the access `head_ms` agrees; factor `0` answers within 50 ms, factor `2` between 364 ms and 430 ms (+ 50 ms); misses are answered within 50 ms; 32 concurrent `h2c` streams of that endpoint all finish within 215 ms + 100 ms of the first (delays run concurrently). |
| MCK-16 | **I.** Switching (§8), with `MIMIC_EGRESS_PULL_INTERVAL=2s`: *to mock*, after a forwarded HTTP/1 keep-alive request, the fake API flips to `mock`; within 2 intervals the next request on the same connection gets a mocked answer, the destination receives no byte after the first exchange and sees its connection closed with a FIN; the same for a new stream on a forwarded `h2c` connection; an open TLS tunnel passed through as `excluded` is closed (`mock_closed`) within one second of the pull, while a non-HTTP tunnel keeps flowing. *Back to discovery*: a mocked HTTP/1 connection gets its pending answer and is then closed, a mocked `h2c` connection gets `GOAWAY`; the next connection reaches the destination, gets its answer unchanged (no `x-mimic-match`) and is recorded in `/discovery`. |
| MCK-17 | **I.** Miss logs (§8): 1000 misses over 3 distinct signatures write exactly 3 WARN `mock miss` lines; 300 distinct signatures write 256 lines and one `mock miss log limit reached`; after a mode change and back, a signature already logged is logged again; `mimic_mock_miss_total` counts all 1300. With canaries in the requests' query, headers and body and in a path segment caught by a detector, no stdout line, `/metrics` series, `/egress` answer, pushed report or mocked answer holds a canary. |
| MCK-18 | **I.** Sidecar memory in mock (§8 "Bounds"), default settings: the sidecar holds a snapshot of just under `MIMIC_EGRESS_MAX_BYTES` (the PUL-18 set) plus one endpoint whose only sample has a 256 KiB JSON response body and whose histogram, with factor 100, always draws the 300 s cap, while the inbound half of the EGR-15 load runs. 4 `h2c` connections of 256 streams and 1 024 HTTP/1 connections (2 048 requests) each post a 256 KiB JSON body to that endpoint: exactly 1 024 are pending (`mimic_mock_pending` 1024) and 1 024 get the overload answer at once (`599`, `mimic_mock_miss_total{reason="overload"}` 1024, one WARN `mock overload`); the bodies beyond `MIMIC_DISCOVERY_CAPTURE_BUDGET` are matched as absent; the peak RSS (`VmHWM`) stays under 120 MiB (8 MiB of headroom under the injected 128 Mi limit), with the allocator settings the webhook injects (`MALLOC_ARENA_MAX=2`, `MALLOC_MMAP_THRESHOLD_=131072`, which the injection test of this row also checks). Then every client aborts: within 1 s `mimic_mock_pending` is 0, nothing is written to the aborted clients, each access line says `client_abort`, and one INFO `mock overload ended` is written. A pull that swaps the snapshot while answers are pending does not raise the peak beyond the bound. |
| MCK-19 | **E.** On kind (after the PUL-19 setup): `front` calls `users` and an HTTPS fake provider in discovery until the egress is pulled; `spec.mode: mock` on the `front` `MimicWorkload` makes, within 2 pull intervals, `status.mode` `{mode: mock, pods: 2, otherPods: 0}`; then `front`'s calls are answered with `x-mimic-match`, the `users` and provider pods log no request, a call to an unrecorded path gets `599`, and a `front` pod created by a scale-up has `MIMIC_MODE=mock` and answers mocked from its first request; `spec.mode: discovery` makes, within 2 pull intervals, `otherPods` 0 again, `users` and the provider receive `front`'s calls and the answers carry no `x-mimic-match`. No canary that `front` sends is in any log line of the cluster. |
| MCK-20 | **C.** A fuzz target feeds arbitrary request parts (method, authority, path, query, headers, body bytes) and egress sets accepted by `EgressSet::from_json` to the signature, lookup, tiers and answer: no panic, and every answer is a miss or satisfies the MCK-04 property. Same CI schedule as DSC-23. |

### 12.12 F11 — Load test of a mocked deployment

Levels as in §12.2. Unit tests build reports with the recorder and the bounded merge (so that
samples, `volatile` and `redacted` are what production gives). `mimic-load` integration tests
run the binary against a fake load API and *fake targets*: an HTTP server per target that
answers the `/load` probe on its own port and the requests on another, adds `x-mimic-load: ok`
(or `refused`, or nothing) and counts accepted connections and requests; "no load" means that
count is 0. Controller integration tests run against a real Valkey and the fake API server of
§12.6, extended with `MimicLoadTest`, `EndpointSlice` and Job objects. Sidecar tests use the
fake sidecar API of F10 to set the mode. A *canary* is as in §12.5. F11 amends INJ-03 (the
`mimic.platform.io/deployment` label, LDT-20); every earlier row must still pass (DSC rows
with no `mimic-load-run` header unchanged).

| ID | Criterion |
|---|---|
| LDT-01 | **U.** Plan (§7.8): only inbound endpoints; a `grpc` endpoint, a `101` one, one without sample and one whose samples all have an unkept request body are skipped `grpc`, `upgrade`, `no_sample`, `opaque_body`; `include` keeps only the listed endpoints and `exclude` wins over it (`excluded`); 2xx/3xx samples are the only usable ones when present, else 4xx/5xx; an endpoint whose only samples carry `Authorization: Bearer <redacted>` is `credentials` without a `header/authorization` field and planned with it; a `<redacted>` path segment is always `credentials`; with 70 usable endpoints the 64 with the most exchanges are kept (ties in key order), the others `limit`; over `MIMIC_LOAD_MAX_PLAN` the samples with the highest content hash go first and emptied endpoints are `plan_size`; weights equal exchange counts. Same report and spec → byte-identical plan; the plan holds no Secret value. |
| LDT-02 | **U.** Request building (§7.8): method, path, query and body of the sample; `host`, `content-length` (recomputed), `content-encoding`, hop-by-hop, `expect` and `proxy-*` headers handled as listed; `host` the endpoint's first host, else `<ip>:<port>`; `mimic-load-run` present, holding the run's ticket exactly as supplied (§7.8, LDT-11, LDT-20). Volatile fields: a `date-time` gets the supplied time in RFC 3339, `550e8400-e29b-41d4-a716-446655440000` keeps the UUID shape with hex letters only, `00-4bf92f…-01` keeps its dashes, an integer keeps its digit count; two random inputs give different values for every volatile field, the same inputs the same request. Credentials replace the whole value of `header/authorization`, `query/api_key` and `body/card/token` with the key's bytes, a trailing newline removed. Property (`proptest`, ≥ 10 000 cases, any recorder-built report and spec): no request holds `<redacted>`, every non-volatile, non-credential value comes from the drawn sample, and endpoint draws follow the weights (χ² at 1 %). |
| LDT-03 | **U.** Schedule (§7.8): open model, rate 100, `rampUp` 10 s, duration 20 s: 500 sends in the first 10 s and 1000 in the next 10 (± 1), none after 20 s; a send due with the pod's concurrency in flight is `missed`, never delayed; closed model: workers rise from 1 to the concurrency over `rampUp`; concurrency 300 and rate 1001 with `MIMIC_LOAD_POD_CONCURRENCY=128` give 3 pods with concurrencies 100/100/100 and rates 334/334/333; a rate lower than the pod count (rate 1 for 2 pods) is `Invalid` naming `spec.rate`. |
| LDT-04 | **U.** Results (§7.8): a value in bucket `b` gives a percentile at most 4.43 % above it; on 100 000 random latencies p50, p90, p99 are within 4.43 % of the exact ones; `max` is the highest bucket bound; merging per-pod results is commutative and associative and equals the results of the union; `rps` is requests × 1000 / elapsed ms rounded; statuses and errors fall in the listed classes only; the serialisation is deterministic (`BTreeMap`) and holds method, port, template and numbers only. |
| LDT-05 | **U.** Validation, gate and Job (§7.8): a field above its `MIMIC_LOAD_*` limit or `rampUp` > `duration` is `Invalid` with the field's name; the gate opens only with effective mode `mock`, at least one target and every target pod's `:pulls` field `mock` within the window (a pod in `discovery`, an expired field, a field more than 60 s in the future or no field keeps it closed); targets keep ready endpoints of pods the §7.2 check accepts for the deployment (`a-b-6d4c8f9b7-xxxxx` is not a target of `a`), one per pod. The Job for concurrency 300: name `mimic-load-<12 hex>`, Indexed, `parallelism` = `completions` = 3, `backoffLimit` 0, the `activeDeadlineSeconds` and `ttlSecondsAfterFinished` of §7.8, owner reference with `blockOwnerDeletion: false`, pod labels and security context of §7.8, `automountServiceAccountToken: false`, the projected token of audience `mimic.platform.io/load`, the Secret volume and the annotation `mimic.platform.io/credentials-secret` (equal to its `secretName`) only when `credentials` is set, image `MIMIC_INJECT_SIDECAR_IMAGE` with command `/usr/local/bin/mimic-load` and no `args`, one container and no init container, `env` of literal §10.6 values only (no `valueFrom`); with `loadTest.networkPolicy` the NetworkPolicy of §7.8 (no ingress rule but the metrics source, egress to `mimic.platform.io/deployment: <dep>` on the plan's ports and 15100, the controller on 8444 and DNS on 53); same input → identical objects. Both pass the LDT-17 policy expressions evaluated in the unit test. |
| LDT-06 | **E.** CRD and admission (§7.8): `deployment` and `duration` are required; `concurrency` defaults to 10, `rampUp` to `0s`, `timeout` to `10s`; `concurrency` 0 and 4097, `rate` 0, `duration` `5s` and `25h`, 65 `include` entries, 33 credential fields and a credential field without a `header/`, `query/` or `body/` prefix are rejected; any change of `spec` but `stop` false → true is rejected, and so is `stop` true → false. A user allowed to create `mimicloadtests` but not to `get` the Secret is refused a `MimicLoadTest` naming it; with `get` it is admitted. Printer columns as §7.8. With `loadTest.enabled=false` (the default) the chart installs no `MimicLoadTest` CRD, no Job, `EndpointSlice` or NetworkPolicy RBAC and neither policy; with `loadTest.enabled=true` and an image given by tag only, `helm template` fails with a message naming `image.digest`. |
| LDT-07 | **I** (controller). Lifecycle (§7.8): a run on a deployment in `discovery` is `Pending`/`WaitingForMock` with no Job, then `Failed`/`NotMock` after `MIMIC_LOAD_START_TIMEOUT` (set to 5 s); once the `MimicWorkload` says `mock` and the target pods' `:pulls` say `mock`, the Job of LDT-05 is created and the run is `Running`; a second run on the same deployment is `Failed`/`Busy`; a third active run in one namespace (`MIMIC_LOAD_MAX_NAMESPACE_RUNS=2`) `Failed`/`NamespaceBusy`; past `MIMIC_LOAD_MAX_RUNS` `Failed`/`ClusterBusy`; with `MIMIC_LOAD_STORE_BUDGET=2Mi` and a 1.5 MiB plan, the second such run on another deployment is `Failed`/`StoreFull` and is admitted once the first ended; with `MIMIC_LOAD_MAX_PENDING=2`, the third `Pending` run of a namespace (by `creationTimestamp`) is `Failed`/`TooManyPending` while the first two keep waiting; two controller processes on one Valkey create one Job per run and never admit more than `MIMIC_LOAD_MAX_RUNS` runs; a value above a limit is `Failed`/`Invalid`; no stored report `NoReport`; no plannable endpoint `NoEndpoints` with `status.skipped`; `MIMIC_LOAD_MAX_RUNS=0` `Disabled`. One INFO `load test started` (namespace, name, deployment, uid, concurrency, rate, duration, targets) and one INFO `load test ended` (phase, reason, requests, errors) per run; `mimic_load_runs_total{phase,reason}` counts each end. |
| LDT-08 | **I** (controller). Load API (§7.8): plan, targets and results answer for a pod of the run's Job with a `mimic.platform.io/load` token; a sidecar token is `401` there and a load token is `401` on `POST /v1/workloads/<dep>/report` and the egress route; a pod name of another Job is `403`, a token of another namespace `404`; a results body over 256 KiB is `413` before it is read; a malformed one `400`; the plan route serves the plan stored at admission even after the report changed, with the ticket and `worker` = the index in the pod name (`mimic-load-…-2-xxxxx` gets 2) and `workers` = the Job's pods; targets follow `EndpointSlice` changes (a pod turning not ready leaves the list) and answer `stop: true` after `spec.stop` or the mode leaving `mock`. Admission order, rate limit and `TokenReview` limits are those of §7.2 (`429` past the rate, no `TokenReview`). |
| LDT-09 | **I** (controller). Results and status (§7.8): cumulative posts from 2 pods are merged into `status.results` as `load::summary` computes; posting the same body twice changes nothing; the run is `Succeeded`/`Completed` once both pods posted `final`, `Aborted`/`ModeLeft` at a final post with that reason; the Job failing is `Failed`/`JobFailed` with the results posted so far; `:load:<uid>` and `:loads` hold the run with the expiry of §7.3 and the 21st run of a deployment evicts the oldest ended one. Size, with a plan of 64 endpoints whose templates are 2 KiB long, posts of exactly 256 KiB and a summary of exactly 192 KiB: while `Running` with 2 pods, the stored bytes of `:load:<uid>` (the sum of `HSTRLEN` over its fields) are at most the plan's bytes + 2 × 256 KiB + 16 KiB; after any end (`Completed`, `ModeLeft`, `Stopped`, `JobFailed`) it holds no `plan`, `ticket` or pod field and its stored bytes are at most 192 KiB + 16 KiB; `MEMORY USAGE` of the key (all samples) is at most 1.25 × those bounds + 16 KiB (allocator size classes); `mimic:v1:loads:reserved` has no field for it and a results post afterwards is `404`. |
| LDT-10 | **I** (controller). Stop (§7.8): while `Running`, setting the `MimicWorkload` to `discovery` deletes the Job within 2 s of the watch event and makes the run `Aborted`/`ModeLeft`; `spec.stop: true` does the same with `Stopped`; in both cases the stop flag is set (and kept) before the Job is deleted, and the run ends in the same pass: the slot is free (a new run on the deployment is admitted) and the generators' next `targets` and `results` calls get `404`, which `mimic-load` treats as a stop (`Stopped`, exit 1, no further post, no retry). |
| LDT-11 | **I** (sidecar). Load marker (§7.8), tickets signed with the root of the test's `MIMIC_TLS_CA`: in `mock`, an inbound HTTP/1 request and an `h2c` stream with a valid ticket naming the sidecar's deployment reach the app without the header, their answers carry exactly one `x-mimic-load` header, `ok`, also when the app's answer sets `x-mimic-load: ok` or `refused` itself, they are counted in `mimic_http_requests_total{direction="inbound"}`, `mimic_load_inbound_total{result="forwarded"}` and `mimic_discovery_exchanges_total{direction="inbound",result="load"}`, their access lines carry `load_run`, and `/discovery` does not change. In `discovery` (`reason=mode`), with a ticket of another deployment or namespace (`workload`), and with a ticket whose payload was changed (another `uid`), whose signature is by another deployment's root, that is expired, malformed, or sent to a sidecar without `MIMIC_TLS_CA` (`ticket`), they get `599`, `x-mimic-load: refused; reason=<reason>`, `content-length: 0`, the app receives no byte, `mimic_load_inbound_total{result="refused_mode"}`, `refused_workload` or `refused_ticket` counts them and one WARN `load request refused` is written per reason per mode period. After a pull switches to `discovery`, the next request on the same keep-alive connection is refused. Requests without the header behave as before. `GET /load` on the admin port answers `{"deployment","mode","namespace","pod"}`; other methods `405`. On HTTP/1 the app receives no byte of a refused request (also none on `h2c`: the connection to the app is opened by the first forwarded stream), the refused connection is closed after the `599`, and the rest is relayed in place: header case and order, bodies, an unmarked or marked request with `Upgrade` (the later `101` tunnel included) are untouched, and a marked request after plain ones, or before an `Upgrade` one, on the same connection is judged on its own. Heads that are forwarded unchanged as non-load: 64 KiB or more without their end (the existing limit behaviour), cut by the client's close (the held bytes are forwarded), never completed (released after the 30 s detection stall), and `mimic-load-run : t` (space before the colon, which the codec does not frame). Without `MIMIC_LOAD_ENABLED=1` (a pod injected while `loadTest.enabled` is false) the header is relayed unchanged, nothing is held back, and `GET /load` answers `404`. |
| LDT-12 | **I** (`mimic-load`). Generation (§7.8), 2 fake targets: rate 200 for 10 s sends between 1900 and 2100 requests spread over both (counted by the fake targets; the run's own clock bounds it, so a slow runner gives fewer sends counted `missed`, never more); closed model with concurrency 8 never has more than 8 in flight; every request carries the marker and fresh volatile values; credentials come from `MIMIC_LOAD_CREDENTIALS_DIR`; results are posted every `MIMIC_LOAD_REPORT_INTERVAL`, cumulative, then once with `final` and `Completed`, and the process exits 0. No connection is opened to any address but the controller and the targets' probe and request ports; a target whose probe answers another deployment, the right deployment with another pod name, `mode: discovery`, `404` or nothing within 2 s receives no connection on its request port; a target whose probe passes but whose request port answers `x-mimic-load: refused; reason=workload` or `reason=ticket` (a connection that landed on another pod) stops the run with `Unverified`. `/metrics` exposes `mimic_load_requests_total{endpoint,result}`, `mimic_load_request_duration_seconds{endpoint}`, `mimic_load_in_flight`, `mimic_load_missed_total` and `mimic_load_targets`, `endpoint` being `<METHOD> <template>` of the plan. |
| LDT-13 | **I** (`mimic-load`). Stop (§7.8): one answer with `x-mimic-load: refused; reason=mode` stops all requests within 2 s of that answer (measured by the fake target: no request arrives later than that, on any target), the final post carries `ModeLeft` and the exit code is 1; an answer without `x-mimic-load` does the same with `Unverified`; a targets answer with `stop: true` stops within one interval; `SIGTERM` sends the final post within `MIMIC_SHUTDOWN_GRACE`; with the controller unreachable for 3 intervals the generator stops (no load without control) with exit 1. FND-01, FND-02 and FND-03 hold for `mimic-load` and its variables of §10.6. |
| LDT-14 | **I** (`mimic-load` and controller). No secret in outputs: with a canary in the credentials Secret, in the fake targets' answer headers and bodies, and in the report's samples where a detector catches it, the requests carry the credential canary exactly in its field, and no stdout line of `mimic-load` or the controller, `/metrics` series, results post, `status` or Valkey value holds a canary, and no stdout line, series, status or read API answer holds the run's load ticket. |
| LDT-15 | **I** (`mimic-load`). Bounds: a plan over `MIMIC_LOAD_MAX_PLAN` is refused (exit 1, no request); with a plan of 64 endpoints whose samples carry 256 KiB bodies and concurrency 128 against slow targets, the peak RSS (`VmHWM`) stays under 200 MiB (the default memory limit is 256 Mi). |
| LDT-16 | **E.** On kind, with `loadTest.enabled=true` and the MCK-19 setup in `mock`: a `MimicLoadTest` on `front` (concurrency 8, rate 50, 60 s, `rampUp` 10 s) runs as a Job in `front`'s namespace whose pod passes Pod Security `restricted`, next to its NetworkPolicy (both gone after the CR is deleted), reaches `Succeeded` with, for each planned endpoint, requests > 0 and `latencyMicros.p99` > 0; during it the `users` and provider pods log no request and `front`'s `status.discovery.exchanges` does not grow. A second run, with `spec.mode: discovery` set 20 s after it started, is `Aborted`/`ModeLeft`, its Job is gone within 10 s, and `users` and the provider receive at most 8 (the concurrency) requests from `front` from then until the end of the check. A run on a deployment in `discovery` stays `Pending` and ends `Failed`/`NotMock` without a pod. No canary of the credentials Secret is in any log line of the cluster. |
| LDT-17 | **E.** RBAC (§11): `kubectl auth can-i` as the controller is `yes` for `create jobs`, `delete jobs`, `delete networkpolicies` and `list endpointslices` in an injected namespace and `patch mimicloadtests/status`, `no` for `get secrets` there, `get pods` and `update mimicloadtests`. As the controller (impersonation), the Job `ValidatingAdmissionPolicy` refuses a Job with another image (another digest, or the tag instead of the digest), `args`, a second container, an init container, an `env` with `valueFrom` or a name outside §10.6, `envFrom`, `automountServiceAccountToken` true, another service account, a projected token with audience absent, empty, the API server's or anything but `mimic.platform.io/load`, or with `expirationSeconds` 3600, a projected source other than `serviceAccountToken`, a `hostPath` volume, `hostNetwork`, `hostPID` or `hostIPC` true, a `hostPort`, `nodeName`, a toleration, `hostAliases`, a privileged or root container, an added capability, a Secret volume not mounted read-only at `/mimic/credentials`, a second Secret volume, a Job without the labels of §7.8, or in a namespace without `mimic.platform.io/inject` `enabled` or `selected`; it refuses a NetworkPolicy of another shape; an ephemeral container cannot be added (no `pods/ephemeralcontainers` permission); the LDT-05 objects are admitted. The policy has `failurePolicy: Fail` and its binding `validationActions: [Deny]`; the same Jobs created by another user are not matched by it (the policy targets the controller only). For a Job it also refuses `priorityClassName`, `runtimeClassName`, `affinity`, `seLinuxOptions` or `appArmorProfile` set, a `schedulerName` other than `default-scheduler`, a `parallelism` other than `completions`, a Secret volume whose `secretName` is not the annotation `mimic.platform.io/credentials-secret` (and the annotation without a Secret volume). A `DELETE` is admitted for the controller's own objects (the §7.8 labels on `oldObject`, also in a namespace that is no longer injected) and refused for a Job or NetworkPolicy without them, while the same `DELETE` by the cluster administrator is not matched. |
| LDT-18 | **C.** A fuzz target feeds reports accepted by `Report::from_json` and arbitrary specs to the plan and request builders: no panic, and every request satisfies the LDT-02 property. Same CI schedule as DSC-23. |
| LDT-19 | **I** (controller). Read API (§7.5): `GET /v1/workloads/<ns>/<dep>/loadtests` lists the stored runs (uid, name, phase, reason, start, end, total) newest first; `…/loadtests/<uid>` answers the run with its stored summary (per-endpoint results); `404` for an unknown uid, `400` for a uid outside the UUID grammar; headers and loopback rules of §7.5. **E.** The deployment page shows a "Load tests" table filled from these routes (pinned headless Chromium, §12.7). |
| LDT-20 | **U.** Load ticket and injection label (§7.8): a ticket signed with a deployment's root verifies with its root certificate's public key and yields the payload; one byte changed in the payload or signature, another deployment's root, an `exp` in the past or a ticket without the `v1.` prefix do not verify; the signed bytes start with `mimic-load-ticket\0`; the same inputs give a ticket that verifies (ECDSA signatures are randomised, the payload is canonical). The injection patch also adds the label `mimic.platform.io/deployment: <deployment>` (INJ-03 amended), creating `metadata.labels` when absent and replacing a value the pod set. |

### 12.13 Monitoring (#378)

Levels as in §12.2. The chart criteria are checked by `scripts/check-chart.sh` on rendered output.

| ID | Criterion |
|---|---|
| MON-01 | **E** (`helm template`, no cluster). With the defaults, the render with `valkey.existingSecret=golden` is byte-identical to `deploy/helm/golden/default.yaml`, which holds no exporter, ACL Secret or PodMonitor. With `valkey.metrics.enabled` the Valkey pod has the container `exporter`, port 9121 named `metrics`, with exactly the arguments of §10.7, an env of `REDIS_USER` (`mimic-exporter`) and `REDIS_PASSWORD` (`secretKeyRef`) and nothing else, `runAsUser` 65532, `runAsNonRoot`, a read-only root filesystem, `drop: [ALL]` and `RuntimeDefault`, and a `livenessProbe` but no `readinessProbe` (#399); the Valkey container has `--aclfile` and no `--requirepass`; its init container `valkey-acl` reads VALKEY_PASSWORD (the Valkey Secret, `valkey.existingSecret` resolved) and EXPORTER_PASSWORD (`valkey.metrics.existingSecret`, or else `<release>-valkey-exporter`), writes the two hash lines of §10.7 with `umask 0226` (mode 0440) into the in-memory volume `acl`, and refuses an empty password; no line says `nopass`, no Secret holds an ACL file, and the render works with `valkey.existingSecret` set (a render without it reads the chart's own Valkey Secret). With `valkey.metrics.existingSecret` set (#399), the render has no `<release>-valkey-exporter` Secret, the exporter's REDIS_PASSWORD and the init container's EXPORTER_PASSWORD read the key `password` of that Secret, and the StatefulSet has no pod annotation (no checksum: a render restarts nothing); without it, the Secret is rendered as before. The Valkey NetworkPolicy admits 9121 from the release namespace and `networkPolicy.metricsNamespaceSelector` only, and no rule of a new object names `secrets` or `nodes/*`. With `metrics.podMonitor.enabled`, the PodMonitors `<release>-sidecar` (selector `mimic.platform.io/deployment` Exists, port `mimic-admin`, target labels `namespace`, `pod`, `deployment`, job `mimic-sidecar`) and `<release>-load` (selector `mimic.platform.io/load-test` Exists, port `mimic-admin`, target label `load_test`, job `mimic-load`) are rendered, with the namespace selector and interval of the values; with `valkey.metrics.enabled` too, `<release>-valkey` selects the Valkey pods on port `metrics` in the release namespace. The default render has none of them. `loadTest.metricsFrom` falls back to the namespace peer of `networkPolicy.metricsNamespaceSelector` (the release namespace when empty) unless set. `helm lint --strict` passes for the default, `podMonitor`, `valkey.metrics`, both, and `loadTest.enabled` with `podMonitor`. |
| MON-02 | **E** (kind, `scripts/e2e.sh` section `mon_02`). After an upgrade with `valkey.metrics.enabled` (the Valkey pod restarts once): the exporter container is Ready; `/metrics` on 9121, read from a pod of the release namespace, answers `redis_up 1`; with a canary key and value written as the default user, that `/metrics` holds neither the canary value nor the exporter password; as `mimic-exporter`, `INFO` answers and `GET` and `SET` of the canary key fail with `NOPERM`; the controller is ready and its status passes still succeed after the restart (`mimic_status_updates_total{result="ok"}` grows); the sidecar `/metrics` of an injected pod answers to a pod of the release namespace on the port named `mimic-admin` (resolved from the pod spec, MON-03), with `mimic_build_info{component="sidecar"}`. After the same upgrade the controller Deployment rolls out Ready, and an unauthenticated `PING` in the Valkey pod answers `NOAUTH` (the password is never printed). The PodMonitor target labels are not checked here: kind has no Prometheus Operator. |
| MON-03 | **U.** The injection patch gives the `mimic-sidecar` container exactly one port, `{"name":"mimic-admin","containerPort":15100,"protocol":"TCP"}`, and the generator Job of §7.8 gives its container the same single port; the patch stays byte-identical for the same input. |
| MON-04 | **I** (`mimic-runtime`, Linux). The `/metrics` of an admin listener holds `process_cpu_seconds_total` (counter), `process_resident_memory_bytes` > 0, `process_open_fds`, `process_max_fds` equal to the soft `RLIMIT_NOFILE`, and `process_start_time_seconds` between the test start minus 60 s and now, each with the §9 help text, no label and no `mimic_` prefix; `process_open_fds` grows by 1 while the test holds one more open file; `process_cpu_seconds_total` does not decrease between two scrapes around a busy loop; a canary set in the process environment and in its arguments is not in the output. The `mimic_*` part of the output is unchanged (FND-03 still passes). |
| MON-05 | **I** (controller). After N requests to `/inject/*` (injected, skipped, untouched, `400` malformed, `413` too large, `405`), `mimic_webhook_admission_duration_seconds_count` is N, with exactly the §9 buckets and no label. **U.** Every reason the webhook can count (each skip and untouched reason of `mimic-core`, each error reason of the webhook) is in the §9 list of its `result`, and the list has no other value. |
| MON-06 | **U** (next to the configuration of each binary). The `mimic_limit` series built from a parsed configuration are exactly the §9 names of that component, sorted, with the parsed values (`MIMIC_SIDECAR_API_EGRESS_BUDGET=16Mi` gives `sidecar_api_egress_budget_bytes` 16777216, `MIMIC_MAX_CONNECTIONS=100` gives `max_connections` 100), the defaults of §10 when unset, and equal outputs for equal inputs. **I** (sidecar). The sidecar's `/metrics` holds them, unchanged across two scrapes. |
| MON-07 | **I** (controller, test Valkey). `mimic_store_deployments` is 0 before the first status pass; after pushes for 3 deployments in 2 namespaces and one status pass it is 3; after one of them is removed from `mimic:v1:workloads` and the next pass, it is 2. |
| MON-08 | **I** (sidecar). With 3 inbound and 2 outbound keep-alive connections open (one of them an opaque TCP tunnel), `mimic_proxy_active_connections` is 3 and 2 per `direction`; within 1 s after they are closed both are 0; with `MIMIC_MAX_CONNECTIONS=2` and 4 inbound connections attempted, the inbound value never exceeds 2. |
| MON-09 | **I** (sidecar). Through an upstream that waits 200 ms before its response head, 20 inbound and 20 outbound HTTP/1.1 exchanges and 20 HTTP/2 streams: `mimic_proxy_overhead_seconds_count` per `direction` equals the exchanges answered, every observation is at most 0.1 (the `le="0.1"` bucket equals the count) while `mimic_http_request_duration_seconds` observes at least 0.2 for each; an opaque tunnel and an `unframed` exchange add no observation. In mock with an egress sample whose latency is 200 ms and factor 1, the overhead of each mocked exchange is also at most 0.1. |
| MON-10 | **U.** The recorder (§6) tells whether a recorded exchange added an endpoint: true for the first `GET /a`, false for a second `GET /a`, true for `POST /a`, true for the exchange whose literal makes `/u/{u_id}` by absorption, false for an exchange that is not recorded (`not_found`, `invalid`, `load`); outbound, the same path under a new authority scope is new. **I** (sidecar, fake sidecar API). `mimic_discovery_last_new_endpoint_timestamp_seconds` is 0 per `direction` at start, set to a time between the request and its scrape after the first inbound exchange (outbound still 0), unchanged by a repeat of the same endpoint before and after an acknowledged push. |
| MON-11 | **I** (sidecar, fake sidecar API). Started with `MIMIC_MODE=discovery` and `MIMIC_MOCK_LATENCY_FACTOR=1.5`, `mimic_mock_latency_factor` is 1.5 and no `mimic_mock_mode_switches_total` series is above 0; pull answers `mock`, `mock`, `mock` with factor 2, then `discovery` give `{to="mock"}` 1 and `{to="discovery"}` 1, and the factor 2 from the third answer on; a restart with the state file holding `mock` counts no switch; with the state directory unwritable, a pull answer `mock` is applied (`mimic_mock_mode` 1) and counted once in `{to="mock"}`. |
| MON-12 | **U.** `load::schedule` planned rate: open model, pod rate 100, `rampUp` 10 s, duration 20 s gives 0 at the start, 50 at 5 s, 100 from 10 s to 20 s, 0 after 20 s; rate 1001 over 3 pods gives 334, 334 and 333 at full rate; the closed model has none. **I** (`mimic-load`, fake load API). During an open-model run `mimic_load_planned_rate` is above 0 and at most the pod's rate; in a closed-model run the family is absent. |
| MON-13 | **E** (kind, folded into `scripts/e2e.sh` section `mon_02`). One scrape of the controller has a `# TYPE` line for every controller metric of the #385 table of §9 and a `mimic_limit` series for each controller name; one scrape of an injected sidecar has the same for the sidecar, `process_open_fds` included although the sidecar is non-dumpable (TLS-16); one scrape of the Valkey exporter has every name of the exporter list of §9. |
| MON-14 | **E** (`helm template`, `scripts/check-chart.sh`). Bundled monitoring off (#379): with the defaults the render is still the golden file of MON-01, and the renders with `metrics.podMonitor.enabled`, `valkey.metrics.enabled` or `loadTest.enabled` hold no object named `<release>-prometheus*` or `<release>-grafana*`. `Chart.yaml` has no `dependencies`, and there is no `Chart.lock` or `charts/` in the chart directory. `helm lint --strict` passes for Prometheus on, Grafana on, both, and both with `valkey.metrics.enabled`, `metrics.podMonitor.enabled` and `loadTest.enabled`. |
| MON-15 | **E** (`scripts/check-pins.sh`, `scripts/check-release-chart.sh`). The Prometheus, Grafana and k8s-sidecar references of `values.yaml` (`monitoring.prometheus.image`, `monitoring.grafana.image`, `monitoring.grafana.sidecarImage`) equal the stages `prometheus`, `grafana` and `k8s-sidecar` of `images/Dockerfile`, and a mismatch (a copy with one digest changed) fails the check. With both enabled, every `image:` of the render of the chart as published has `@sha256:`. |
| MON-16 | **E** (`helm template`, `scripts/check-chart.sh`). With `monitoring.prometheus.enabled`: the Deployment `<release>-prometheus` has 1 replica, strategy `Recreate`, the image of the values, exactly the six arguments of §10.7 (none of the four disabled flags), a checksum annotation that changes when `metrics.podMonitor.interval` changes, `runAsUser` and `runAsGroup` 65534, `runAsNonRoot`, a read-only root filesystem, `allowPrivilegeEscalation: false`, `drop: [ALL]`, `RuntimeDefault`, the resources of the values, an emptyDir with `sizeLimit` `1Gi` mounted at `/prometheus`, the ConfigMap mounted read-only at `/etc/prometheus`, and probes on `/-/ready` and `/-/healthy`; the Service `<release>-prometheus` is `ClusterIP` with the single port 9090. |
| MON-17 | **E** (`helm template`, `scripts/check-chart.sh`). The jobs of `prometheus.yml` are exactly those of the §10.7 table: names, `role: pod`, label selectors, namespaces (the release namespace; for `mimic-sidecar` and `mimic-load` none, i.e. all, or the set of `monitoring.prometheus.namespaces` plus the release namespace), port names, path `/metrics`, intervals, the drop of `Succeeded` and `Failed` pods, and target labels; `mimic-valkey` is present only with `valkey.metrics.enabled`. Equivalence: with `metrics.serviceMonitor.enabled`, `metrics.podMonitor.enabled` and `valkey.metrics.enabled` also on, for each ServiceMonitor and PodMonitor the job of the same `job` value has the same selector labels, port name, path, interval and the same `(source labels, target label, replacement)` relabel rules as the monitor, except `namespace` and `pod`: those are the Operator's implicit target labels, so the ServiceMonitor and the PodMonitors carry no relabelings for them (the ServiceMonitor has none at all, the PodMonitors name them only where §12.13 MON-01 lists them), and the comparison leaves them out, while `prometheus.yml` sets them for every job; a monitor changed in a copy of the chart without its job fails the check. No job scrapes a node, the kubelet, cAdvisor, the API server or Prometheus, and the file has none of `authorization`, `basic_auth`, `bearer_token`, `bearer_token_file`, `oauth2`, `tls_config`. |
| MON-18 | **E** (`helm template`, `scripts/check-chart.sh`). RBAC: with `monitoring.prometheus.enabled` and `namespaces: []`, one ClusterRole `<release>-prometheus` with the single rule `apiGroups: [""]`, `resources: [pods]`, `verbs: [list, watch]`, bound by a ClusterRoleBinding to the ServiceAccount `<release>-prometheus`, and no Role; with `namespaces: [a, b]`, a Role and a RoleBinding with that rule in `a`, `b` and the release namespace (each once, also when the release namespace is listed) and no ClusterRole. With `monitoring.grafana.enabled`, the Role `<release>-grafana-dashboards` in the release namespace has the single rule `get`, `list`, `watch` on `configmaps`, its RoleBinding names the ServiceAccount `<release>-grafana`, and no ClusterRole or other Role names that ServiceAccount. Over every render of MON-14, no rule names `secrets`, `nodes`, `nodes/*`, `pods/exec`, `pods/log`, `pods/portforward` or `*`, or a verb other than `get`, `list`, `watch` in a monitoring object. |
| MON-19 | **E** (`helm template`, `scripts/check-chart.sh`). With `monitoring.grafana.enabled`: the Deployment `<release>-grafana` has 1 replica, strategy `Recreate`, exactly the containers `grafana` and `dashboards` with the pinned images of the values, no init container, a checksum annotation that changes with `grafana.ini` and with the provisioning, `automountServiceAccountToken: false`, and the projected token volume mounted in `dashboards` only. The Secret `<release>-grafana-admin` has a 32-character alphanumeric `admin-password` (its template reads the existing Secret with `lookup` first, as the Valkey one does; kept across upgrades, MON-21); it is mounted read-only in `grafana` only, `grafana.ini` sets `security.admin_password` to `$__file{/etc/grafana-admin/admin-password}` (the `grafana` container has no env), no env of any container holds or references the password, and the password appears in no other rendered object. The rendered `grafana.ini` holds every setting of §10.7, and its `plugins.disable_plugins` lists every bundled datasource plugin but `prometheus`, the legacy `postgres` ID included (#407); the provisioned datasource has `uid` `mimic-prometheus`, `type` `prometheus`, URL `http://<release>-prometheus:9090` (the value of `monitoring.grafana.datasourceUrl` when set) and `editable: false`; the provider reads `/tmp/dashboards`; the `dashboards` container has `LABEL=grafana_dashboard`, `LABEL_VALUE=1`, `RESOURCE=configmap`, `NAMESPACE` the release namespace and no `REQ_URL`, `REQ_USERNAME` or `REQ_PASSWORD`. The Service `<release>-grafana` is `ClusterIP` with the single port 3000; there is no Ingress, HTTPRoute or PersistentVolumeClaim; `grafana` mounts a memory emptyDir on `/tmp`; the pod has `fsGroup` 472 and `supplementalGroups` [472] (§10.7: the sidecar writes the emptyDir of grafana's dashboards); both containers have `runAsNonRoot`, the UIDs of §10.7, a read-only root filesystem, `allowPrivilegeEscalation: false`, `drop: [ALL]`, `RuntimeDefault` and the resources of the values (Pod Security `restricted`, FND-08). |
| MON-20 | **E** (`helm template`, `scripts/check-chart.sh`). With `networkPolicy.enabled`, Grafana on and `datasourceUrl` set, the Grafana NetworkPolicy has the rules of `datasourceEgress` after its DNS and API server rules, and with `datasourceUrl` empty it has none of them; with `datasourceUrl` empty the 9090 rule admits the bundled Prometheus only (MON-20 covers both cases). With `networkPolicy.enabled` and Prometheus on, the NetworkPolicy `<release>-prometheus` selects the Prometheus pods with `policyTypes: [Ingress, Egress]`; its ingress is TCP 9090 from the Grafana pods of the release when Grafana is on, and empty otherwise; its egress is exactly TCP 15100 to every namespace (or to the namespaces of `monitoring.prometheus.namespaces` and the release namespace), TCP 8081 to the controller pods, TCP 9121 to the Valkey pods only with `valkey.metrics.enabled`, and TCP 443 and 6443 to `networkPolicy.apiServerCIDR` (no peer when empty). With Grafana on, `<release>-grafana` selects the Grafana pods with both types; ingress TCP 3000 from the release namespace; egress exactly TCP 9090 to the Prometheus pods, UDP and TCP 53 to `loadTest.dnsSelector`, and TCP 443 and 6443 as above. With `networkPolicy.enabled: false` neither is rendered. With `loadTest.enabled` and Prometheus on, `loadTest.metricsFrom` empty falls back to the namespace peer as with `metrics.podMonitor.enabled` (MON-01), and a set value wins. The controller and Valkey NetworkPolicies are the same as with monitoring off. |
| MON-21 | **E** (kind, `scripts/e2e.sh` section `mon_21`, after `mon_02`). After an upgrade with `monitoring.prometheus.enabled` and `monitoring.grafana.enabled` (and `valkey.metrics.enabled` of `mon_02`), checked from pods of the release namespace. The Prometheus API is reached from a pod that the Prometheus NetworkPolicy admits: a pod carrying exactly the labels of that policy's ingress selector (the Grafana pods' labels, read from the live NetworkPolicy, not hard-coded). Within 120 s Prometheus `/-/ready` answers 200 from that pod and `/api/v1/targets` lists the `<release>-controller`, `mimic-sidecar` and `mimic-valkey` targets with health `up`, the injected pod's target carrying its `namespace`, `pod` and `deployment`; `POST /-/reload` and `POST /api/v1/admin/tsdb/snapshot` from that pod answer non-2xx. An unlabelled pod of the release namespace gets no HTTP answer from port 9090 (a timeout or a connection failure, status 0), so the policy is enforced, not vacuous. Grafana `/api/search` without credentials answers 401; with the admin credentials (read from the Secret into a variable, never printed) `/api/datasources/uid/mimic-prometheus/health` answers status `OK`, and a dashboard (uid `mimic-e2e`) put in a ConfigMap labelled `grafana_dashboard: "1"` by the test is listed by `/api/search` within 60 s, while one labelled `"0"` is not. The admin password appears in no log of any container of the release namespace (`grep -qF`, never printed), and a second `helm upgrade` with the same values keeps it (the sha256 of the Secret's data is unchanged). `kubectl auth can-i get secrets` answers `no` as the Prometheus and the Grafana ServiceAccounts, in the release namespace and in the app namespace.; the default login `admin:admin` gets 401. |

## 13. Test strategy

- **Unit** tests in `mimic-core` (pure logic) and next to the code.
- **Integration** tests per crate in `tests/`: in-process routers, or the real binary started
  as a child process. From F5, controller store tests use a real Valkey (§12.6); the fake RESP
  server of FND-06 stays for readiness only.
- **End-to-end** on kind with the Helm chart (`scripts/e2e.sh`), locally and in CI. From F6
  the UI is also checked in a pinned headless Chromium (§12.7), the only browser test.
- A criterion is covered when a test whose name contains its ID (lower case) passes.
- A timing bound ("within 1 s") is measured from the event that triggers it (the close, the
  signal), not from the start of the test, and the test's own wait for that event has its own
  larger timeout, so that a slow runner fails only when the sidecar is really late.
