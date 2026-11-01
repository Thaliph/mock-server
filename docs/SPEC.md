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
   being rate-limited.

### 1.1 In scope (v1)

- HTTP/1.1, HTTP/2 and unary gRPC, cleartext or TLS, inbound and outbound.
- Discovery mode (record) and mock mode, switched per deployment.
- Access logs, detailed component logs, Prometheus RED metrics for ingress and egress.
- A read-only UI per deployment (OpenAPI + Swagger UI).

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
| `mimic-controller` | Deployment in `mimic-system` | Mutating webhook (injection), `MimicWorkload` reconciler, sidecar API (§7.2), UI. Stateless: all state is in Valkey, except the TLS seed kept in a Secret (§5). |
| Valkey | StatefulSet in `mimic-system` | Shared store: merged endpoints and egress recordings per deployment. AOF persistence on a PVC. |

Crates: `mimic-core` (pure logic: no tokio, no I/O, no clock), `mimic-runtime` (shared process
plumbing: configuration, logs, metrics, health, shutdown), `mimic-sidecar`, `mimic-controller`,
from F1 `mimic-init` and from F3 `mimic-trust`. `mimic-core` may depend on pure data crates
(`serde`, `serde_json`, `k8s-openapi`, `rustls-pki-types`).

## 3. Modes

| Mode | Inbound | Outbound |
|---|---|---|
| `discovery` (default) | forwarded to the app, recorded | forwarded to the destination, recorded |
| `mock` | forwarded to the app, recorded | answered from recordings; never forwarded (§8) |

The mode is set per deployment on its `MimicWorkload` (`spec.mode`). Every pod of the deployment
follows it, including pods created later. Entering mock: the sidecar pulls the deployment's
egress recordings from the controller, then switches. Leaving mock: the sidecar switches back to
discovery immediately.

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
- **Upgrades**: a request answered with `101` (WebSocket, `h2c` upgrade), or a `CONNECT`
  answered with 2xx, continues as an opaque tunnel after the response. After a request that
  asks for an upgrade or is a `CONNECT`, the relay reads no more client bytes until the final
  response to it arrives (the client must wait for it, RFC 9110 §7.8, §9.3.6). A declined
  upgrade leaves a plain HTTP/1 connection. The relay holds at most one 16 KiB read, the rest
  stays in the socket; a codec fed more than 64 KiB behind an unanswered upgrade (without the
  relay's flow control) reports the upgrade request as `unframed`, drops its
  copy and uses every byte: none is lost.
- **Failures**: the sidecar never synthesises a response. When the destination refuses the
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
  tunnels run until `MIMIC_SHUTDOWN_GRACE` ends. When it ends, the sidecar closes every
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
  | `result` | `ok`, `upstream_reset`, `client_abort`, `unframed` (HTTP/1 the codec cannot frame), `rejected` (HTTP/2 connection error), `shutdown` (closed at the end of the grace) | `ok`, `upstream_reset`, `client_abort`, `shutdown` |
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
  destination (it issues the leaf for the SNI and picks `h2` when offered, else `http/1.1`),
  and a connection that the table above would pass through is closed instead, without any
  byte towards the destination (the real destination is never called, §3).

## 6. Discovery

The sidecar turns the HTTP exchanges it relays into a description of the endpoints, keeps that
description in a bounded in-memory **local report**, and (from F5) ships it to the controller,
which merges the reports of every pod of the deployment in Valkey. F4 records **inbound**
exchanges (the endpoints the deployment exposes); F7 runs the same engine on outbound ones.
Everything below except the capture tap, content decoding and the recorder thread is pure logic in `mimic-core`
(`discovery` module): parsing, templating, inference, redaction, sampling, merging.

- **What is recorded.** Every inbound HTTP/1 and `h2` exchange that got a response head, except
  probe requests (§4, they never reach the inbound listener), `unframed` and `rejected`
  exchanges, and exchanges whose path is over 2048 bytes or 32 segments (`invalid`). An
  exchange answered `404` or `405` creates no endpoint: it is counted on the endpoint whose
  template already matches it, else dropped as `not_found` (scanners must not fill the
  report). Inbound HTTPS is not decrypted (§5), so it is not discovered. An upgraded exchange
  (`101`, WebSocket) is recorded without bodies.
- **Endpoint key**: `direction`, `port`, `method`, path template. For inbound, `port` is the
  original destination port (the app's listening port), not the `Host`: clients reach the
  same API through the Service name, its FQDN or the pod IP. The normalised authorities seen
  (§4.1) are kept per endpoint as a set of at most 16 (`hosts`, first 16 in byte order). F7
  keys outbound endpoints by authority instead of port.
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
  are one parameter (OpenAPI cannot hold `/a/{x}` and `/a/{y}`); its schema is the merge of the
  values' schemas (§6 schemas). A parameter is named after the previous literal segment, `{<literal>_id}`
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
    included) becomes the marker, except: `authorization` keeps its scheme
    (`Bearer <redacted>`); `cookie` keeps cookie names (`a=<redacted>; b=<redacted>`);
    `set-cookie` keeps the name and attributes.
  - *By value* (every other string: header values, query values, path segments, JSON strings
    and numbers): a JWT (three base64url parts, the first decoding to a JSON object with
    `alg`), a PEM block, a `Bearer `/`Basic ` credential, a 13–19 digit card number (spaces or
    dashes allowed) that passes Luhn, an AWS access key id (`AKIA`/`ASIA` + 16), and tokens with
    a known prefix (`ghp_`, `gho_`, `ghu_`, `ghs_`, `ghr_`, `github_pat_`, `glpat-`,
    `xox[abprs]-`, `sk_live_`, `rk_live_`, `sk-`); a URL's userinfo password. A path segment and a query value are percent-decoded for the
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
- **Local report.** A JSON document (`"version": 1`), serialised with sorted keys
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
- **Bounds.** At most `MIMIC_DISCOVERY_MAX_ENDPOINTS` endpoints per sidecar (further new
  endpoints are dropped, `endpoint_limit`, one WARN `endpoint limit reached`); the template tree
  keeps its shape for the life of the process (counters and samples are what F5 ships and
  resets). Capture never slows forwarding: the proxy copies at most the head (64 KiB, §4.1) and
  256 KiB of each body into an exchange record while relaying, within the global in-flight
  budget `MIMIC_DISCOVERY_CAPTURE_BUDGET` (a body that does not fit is not captured,
  `budget`), and hands the record to a bounded queue of `MIMIC_DISCOVERY_QUEUE` records,
  drained by one recorder thread that does the decoding, inference and merge. A full queue
  drops the record (`dropped`). Forwarding never waits for discovery (§4.1: forward, never
  refuse).
- **HTTP/1 tap (F4 step 6).** Only inbound exchanges of plain HTTP/1 connections are
  captured. The codec hands the raw head and the first 256 KiB of the body payload (chunk
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
  Key layout, retention (30 days) and the API are in §7.3 and §7.2.
- **Consumers.** F6 renders reports as OpenAPI 3.1. F7 records egress with the same engine.
  F10 mock matching ignores volatile and redacted fields (§8). F11 (load test) builds requests
  from ingress templates, samples, schemas and volatility; redacted values (credentials) must
  be supplied by the user, and its own traffic must not be recorded as discovery (F11
  design).

## 7. Controller and storage

- `MimicWorkload` (namespaced CRD, `mimic.platform.io/v1alpha1`): selects the deployment named
  by `spec.deployment` (required, DNS-1123 name, immutable) in its namespace, holds
  `spec.mode` (`discovery` | `mock`, default `discovery`), `spec.tls.exclude` (F3, §5),
  `spec.discovery.redact` (F4, §6: extra names redacted by name, at most 64, each of `a-z`, `A-Z`, `0-9`, `-`, `_`,
  lower-cased by the webhook, applied like `spec.tls.exclude` to pods created afterwards),
  latency settings; `status` reports the deployment's discovery counts and pushing pods
  (F5, §7.4), later mode convergence (F10). F1 introduces the CRD with `spec.deployment` and `spec.mode` only; other fields arrive
  with their feature. When several `MimicWorkload`s name the same deployment, any of them
  selects it for injection; which one sets the mode is decided in F10.
- Sidecars push their local recordings periodically (§6 "Shipping", §7.2) and, from F9/F10,
  pull the deployment's mode and, in mock mode, its aggregated egress recordings. Valkey keys
  are namespaced by `<namespace>/<deployment>` (§7.3).
- The controller merges endpoints of all pods of a deployment (same endpoint key → merged
  counters, histograms, schemas, sample sets) with the pure merge of §6, which also
  re-applies path templating on the union, within the store bounds of §7.3.
- The UI and its API are read-only and served on the controller's UI listener, bound to
  loopback and reached by `kubectl port-forward` (Kubernetes RBAC `pods/portforward` in the
  controller's namespace is the authentication, §7.5).

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
  - `mimic-sidecar` mounts `mimic-trust` at `/mimic` (read-write) and `mimic-token` at
    `/var/run/secrets/mimic.platform.io` (read-only), and gets `MIMIC_TLS_CA` (the
    deployment's root certificate, PEM), `MIMIC_TLS_EXCLUDE` (§5), `MIMIC_CONTROLLER_URL`
    (`https://<webhook service>.<namespace>.svc:<sidecar API port>`) and `MIMIC_CONTROLLER_CA`
    (the webhook CA, PEM). Before it is ready it copies the static `mimic-trust` binary to
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
  `{"session": "<32 lower-case hex>", "sequence": <1 … 2^53>, "report": <§6 report>}`. The
  deployment is in the path, so it is authorised before the body is read. → `200`
  `{"applied": true}` once the push is merged into the deployment's store (§7.3), or
  `{"applied": false}` when this pod and session already had this or a later sequence
  applied (a retry: nothing changes). `401`/`403` as above; `400` for a missing
  `Content-Length`, a malformed body, session or sequence, or a report that fails the
  structural check of §7.3; `413` when `Content-Length` is over
  `MIMIC_SIDECAR_API_MAX_REPORT`, answered before the body is read; `405` for another method;
  `429` over the rate; `507` when the report has more than `MIMIC_STORE_MAX_PUSH_ENDPOINTS`
  endpoints in a direction (the push is refused whole, never truncated) or when the
  deployment is new and its namespace already holds `MIMIC_STORE_MAX_DEPLOYMENTS`
  deployments; `503` with `Retry-After: 1` when the bodies being
  read would exceed `MIMIC_SIDECAR_API_BODY_BUDGET`, Valkey is unavailable, the write lost 5
  optimistic attempts (§7.3) or the `TokenReview` fails.
- **Shipping client** (sidecar, F5). Every `MIMIC_DISCOVERY_PUSH_INTERVAL` (the first one after
  a random delay within one interval, so that the pods of a rollout do not push together),
  the sidecar freezes the report recorded since its last acknowledged push into a *pending*
  report with the next sequence (nothing is frozen when it covers no exchange) and posts it;
  new exchanges go into a fresh *current* report. The session is 128 random bits drawn at
  process start, so a restarted sidecar starts a new session at sequence 1. A pending report
  is retried unchanged (same body) until it is settled: `200` settles it; `413` removes its
  samples and posts it again with the same sequence (it was not applied); `400`, `405` and
  `507` drop it with one WARN `report rejected` (`status`); anything else (connection error,
  no answer within 30 s, `401`, `403`, `429`, `5xx`) retries with the backoff of §5 (1, 2,
  4 … 60 s, or `Retry-After` when longer). No new report is frozen while one is pending.
  Pending and current samples share `MIMIC_DISCOVERY_SAMPLE_BUDGET`. At `SIGTERM`, once the
  proxy has drained, the sidecar posts the pending then the current report within what
  remains of `MIMIC_SHUTDOWN_GRACE`; what is not acknowledged then is lost, with one WARN
  `report not shipped` (`exchanges`). Without `MIMIC_CONTROLLER_URL` nothing is shipped (one
  INFO `report shipping disabled` at start) and the report accumulates as in F4. `GET
  /discovery` serves the current report. Logs (`target` = `discovery`): DEBUG `report pushed`
  (`sequence`, `exchanges`, `bytes`, `applied`), WARN `report push failed` (`status` or
  `error`, `attempt`) once per failure streak, INFO `report push recovered`; never the token.
- One log line per request (`target` = `sidecar_api`): `endpoint` (the route, e.g.
  `/v1/workloads/{deployment}/report`), `namespace`, `pod`, `deployment`, `result` (`ok`,
  `duplicate`, `unauthenticated`, `forbidden`, `invalid`, `too_large`, `rate_limited`,
  `push_limit`, `deployment_limit`, `unavailable`), plus for a push `session`, `sequence`, `bytes`, and for
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
  | `mimic:v1:{<ns>/<dep>}:report` | string | merged report: canonical §6 JSON, `pod` null, with `last_seen` | `MIMIC_STORE_TTL` after the last applied push |
  | `mimic:v1:{<ns>/<dep>}:meta` | hash | `version` (write counter), `updated` (ms of the last applied push), `exchanges`, `endpoints_inbound`, `endpoints_outbound`, `sample_bytes`, `report_bytes` | same |
  | `mimic:v1:{<ns>/<dep>}:pods` | hash | field `<pod>` → ms of its last push (applied or not) | each field min(24 h, `MIMIC_STORE_TTL`) after its last write |
  | `mimic:v1:{<ns>/<dep>}:seq` | hash | field `<pod>/<session>` → last applied sequence | same as `:pods` fields |
  | `mimic:v1:ns:<ns>` | sorted set | member `<dep>`, score = `updated` | `MIMIC_STORE_TTL` after the last write; members older than it removed by each write |
  | `mimic:v1:workloads` | sorted set | member `<ns>/<dep>`, score = `updated` | members older than `MIMIC_STORE_TTL` removed by each write |

- **Push processing**: structural check of the posted report, then re-redaction, then
  stamping, then the write. Nothing of a push is stored or logged before its re-redaction.
  - *Structural check* (pure, `mimic-core`): `"version": 1` and every bound of §6 — at most
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
    Any other violation is `400` with its `reason`.
  - *Re-redaction* (pure, idempotent): the §6 redaction applied again to every sample, host,
    concrete path and volatile field with the built-in rules and the union of
    `spec.discovery.redact` of the `MimicWorkload`s naming the deployment (from the
    controller's cache, so a name added after the pod started applies); a field that becomes
    redacted loses its sketch and is marked `redacted`, a sample that changes is keyed by the
    hash of its new content. A sidecar of an older version, or a faulty one, cannot put a
    value into Valkey that the controller's rules redact.
  - *Stamping*: every endpoint and sample of the push gets `last_seen`, the current UTC day
    (days since 1970-01-01); a merge keeps the larger value. Sidecar reports carry no
    `last_seen`.
  - *Write*: within a replica, one writer per deployment; pushes that arrive while it writes
    are merged into its next write (group commit), and each is answered after the write that
    holds it. At most 2 deployments are written at once per replica (memory bound). A write
    reads `:report`, `:meta` `version` and the `:seq` fields of its pushes; answers
    `applied: false` for a push whose sequence is not above the stored one of its pod and
    session; merges the others into the stored report with the **bounded merge** below;
    drops endpoints and samples whose `last_seen` is older than `MIMIC_STORE_TTL` (in whole
    days); and runs one Lua script that, only if `version` and the `:seq` fields are still
    those read, writes `:report`, `:meta` (`version` + 1), `:seq`, `:pods` and both indexes,
    refreshes the expiries and prunes the indexes, refusing a new deployment past
    `MIMIC_STORE_MAX_DEPLOYMENTS` in the namespace (`507`). Otherwise nothing is written and
    the write starts again (at most 5 attempts, 10–100 ms random backoff, then `503`). So a
    push is applied once, whichever replica receives it and however often it is retried
    (within the `:seq` field expiry, §11.1). A stored value that does not parse or fails the
    structural check is never overwritten: ERROR `stored report unreadable` (`namespace`,
    `deployment`), push `503`, read `500`.
- **Bounded merge** (pure, `mimic-core`): the §6 merge under the store limits
  (`MIMIC_STORE_MAX_LITERALS`, `MIMIC_STORE_MAX_ENDPOINTS` per direction,
  `MIMIC_STORE_SAMPLE_BUDGET` per deployment); counters saturate at 2^64 − 1. The merge
  re-applies templating on the union: a node that the deployment sees above the literal limit
  absorbs. Below the limits the stored report does not depend on the order or grouping of
  pushes. At a limit, what is kept is chosen by traffic, never by key order alone, so that
  endpoints that sort first (`/!`, `/0`, the paths of a scanner the app answers `2xx`) cannot
  crowd out the deployment's real API:
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
    and nothing is evicted. Counters, histograms, schemas and volatility merge whatever is
    refused or evicted.
  Ties left by those rules are broken by the full endpoint key. A stored report already over
  the budget (a lowered `MIMIC_STORE_SAMPLE_BUDGET`) is trimmed by the same eviction rule; a
  sample added and evicted within one merge counts as `evicted`.
  Refusals are counted in `mimic_store_refused_total{kind,reason}` with one WARN
  `store limit reached` (`namespace`, `deployment`, `reason`) per deployment and reason per
  process; evictions in `mimic_store_evicted_total{kind}` with one WARN `store evicted`
  (`namespace`, `deployment`, `kind`, `count`) per deployment and kind per process.
- **Pod churn**: no pod is read. A deleted pod's data stays merged (the deployment's API is
  the union over its life) and it ships its last report at `SIGTERM`; the pods of a rolling
  update push into the same key; a deleted pod's `:pods` and `:seq` fields expire after
  min(24 h, `MIMIC_STORE_TTL`). A deployment's keys outlive it until `MIMIC_STORE_TTL`, and a
  deployment created again with the same name continues them.
- **Valkey memory**: the chart runs Valkey with `maxmemory` (75 % of its memory limit) and
  `maxmemory-policy noeviction`: a full Valkey refuses writes (pushes get `503`, sidecars
  keep merging locally within their bounds) instead of evicting one key of a deployment and
  breaking its consistency. Chart defaults: Valkey memory limit `1Gi` and PVC `4Gi` (AOF
  rewrites need about twice the data), controller memory limit `512Mi` (two writes of a
  deployment report at once plus `MIMIC_SIDECAR_API_BODY_BUDGET`).

### 7.4 `MimicWorkload` status (F5)

- The CRD has a `status` subresource. Every `MIMIC_STATUS_INTERVAL` each controller replica
  computes, for each `MimicWorkload` of its cache, from the `:meta` and `:pods` keys of its
  deployment (a pure function of them, the window and the current time):

  ```yaml
  status:
    discovery:
      endpoints: {inbound: 12}               # per direction; directions without any omitted
      exchanges: 34567
      sampleBytes: 1048576
      pods: 2                                # pods whose last push is within MIMIC_STATUS_POD_WINDOW
      lastPushTime: "2026-11-01T10:00:00Z"   # last applied push, UTC, seconds; absent without data
  ```

  and applies it by server-side apply on the status subresource (field manager
  `mimic-controller`) only when it differs from the status in its cache. Both replicas
  compute the same value except at a window edge, and an apply of an equal value changes
  nothing, so no leader is elected. A deployment without data gets zero counts and no
  `lastPushTime`. While Valkey is unavailable the status is left as it is (one WARN
  `status not updated` until it recovers). `kubectl get mimicworkload` prints `Mode`,
  `Endpoints` (sum), `Pods` and `Last push`. Mode convergence is added in F10.

### 7.5 Read API (F5)

- Plain HTTP on `MIMIC_UI_ADDR` (default `127.0.0.1:8080`, loopback: reached only through
  `kubectl port-forward` to a controller pod); a peer that is not a loopback address gets
  `403` whatever the bind address. `GET` only (`405` otherwise), JSON, sorted keys:
  - `GET /v1/workloads` → `{"workloads": [{"namespace", "deployment", "endpoints",
    "exchanges", "sample_bytes", "last_push"}]}`, sorted by namespace then deployment;
  - `GET /v1/workloads/<ns>/<dep>` → the same object plus `"pods": {"<pod>": "<last push,
    RFC 3339>"}`;
  - `GET /v1/workloads/<ns>/<dep>/report` → the stored `:report` bytes.

  `404` for a deployment without data, `400` for a name that is not DNS-1123, `503` while
  Valkey is unavailable. No write and no delete: data leaves by expiry (§7.3).
- Consumers: F6 adds the OpenAPI 3.1 document and Swagger UI of a deployment on this
  listener, built from the same report; F9 adds an authenticated read of a sidecar's own
  deployment on the sidecar API (§7.2) for egress recordings and the mode (F10); F11 reads
  ingress templates, samples, schemas and volatility through the same `store` module, its
  access path being part of its design.

## 8. Mock

- Matching, first hit wins: (1) exact: method, authority, path, query, normalised body;
  (2) same endpoint, closest body ignoring volatile fields; (3) most frequent response of the
  endpoint. The response carries `x-mimic-match: exact|endpoint|fallback`. Volatile and
  redacted fields (§6) are ignored by (2) and never compared by (1).
- Latency: drawn from the endpoint's recorded histogram; `spec.mock.latencyFactor` scales it
  (`0` disables it).
- No match: status **599**, header `x-mimic-miss: 1`, metric `mimic_mock_miss_total`, one log
  line with the request signature. The real destination is never called.

## 9. Observability

- **Logs**: JSON lines on stdout, one object per event, with at least `timestamp`, `level`,
  `target` and `message`. Access logs (one line per exchange, `target` = `access`) and
  component logs. Level from `MIMIC_LOG` (`tracing` filter syntax, default `info`).
- **Metrics**: OpenMetrics text format (Prometheus-compatible) on the admin port at `/metrics`. Every component exposes
  `mimic_build_info{component,version} 1` and `mimic_log_lines_dropped_total` (§4.1). The
  sidecar exposes RED metrics with bounded cardinality:

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
  | `mimic_discovery_exchanges_total` | counter (F4), HTTP exchanges seen by discovery | `direction`, `result` (`recorded`, `not_found`, `invalid`, `endpoint_limit`, `dropped`, `panic`) |
  | `mimic_discovery_bodies_total` | counter (F4), bodies of recorded exchanges | `direction`, `part` (`request`, `response`), `result` (`parsed`, `empty`, `opaque`, `too_large`, `encoding`, `invalid`, `budget`) |
  | `mimic_discovery_samples_total` | counter (F4), sample admissions | `direction`, `result` (`kept`, `replaced`, `rejected`, `budget`) |
  | `mimic_discovery_redactions_total` | counter (F4) | `location` (`path`, `query`, `header`, `body`), `rule` (`name`, `jwt`, `pem`, `credential`, `card`, `aws`, `token_prefix`, `userinfo`) |
  | `mimic_discovery_endpoints` | gauge (F4), endpoints in the template tree | `direction` |
  | `mimic_discovery_sample_bytes` | gauge (F4), bytes held by samples | — |
  | `mimic_discovery_pushes_total` | counter (F5), settled or failed push attempts (§7.2) | `result` (`ok`, `duplicate`, `too_large`, `rejected`, `error`) |
  | `mimic_discovery_last_push_timestamp_seconds` | gauge (F5), time of the last acknowledged push, 0 before | — |

  An intercepted connection is counted in `mimic_proxy_connections_total` and
  `mimic_proxy_bytes_total` under its decrypted protocol (`http1`, `h2`, or `tcp` for an
  opaque decrypted tunnel), with the SNI as `authority` when present; a passed-through one
  under `tls` as before. The controller exposes `mimic_sidecar_api_requests_total` (§7.2) and
  `mimic_tls_seed_loaded` (gauge, 1 once the seed is loaded), and from F5 (no label carries a
  namespace, deployment or pod, so cardinality stays fixed):

  | Metric | Type | Labels |
  |---|---|---|
  | `mimic_store_writes_total` | counter, store write attempts (§7.3) | `result` (`ok`, `conflict`, `error`) |
  | `mimic_store_write_duration_seconds` | histogram, one write from read to script result; buckets 0.005 … 10 as `mimic_http_request_duration_seconds` | — |
  | `mimic_store_report_bytes` | histogram, size of each report written; buckets 4Ki, 16Ki, 64Ki, 256Ki, 1Mi, 4Mi, 16Mi, 64Mi | — |
  | `mimic_store_pushes_total` | counter, pushes by outcome | `result` (`applied`, `duplicate`) |
  | `mimic_store_refused_total` | counter, growth refused by the bounded merge | `kind` (`endpoint`, `sample`), `reason` (`endpoint_limit`, `budget`) |
  | `mimic_store_evicted_total` | counter, stored endpoints and samples displaced by the bounded merge (§7.3) | `kind` (`endpoint`, `sample`) |
  | `mimic_store_expired_total` | counter, endpoints and samples dropped by retention | `kind` |
  | `mimic_status_updates_total` | counter, `MimicWorkload` status applies | `result` (`ok`, `error`) |

  `authority` is the normalised target authority of §4.1 (lower case, no trailing dot, default
  port removed, `(invalid)` when it is not a host name or IP literal; an HTTP/1 `Host` value or
  request-target authority is first cut to 1024 bytes, the codec's memory bound). Per direction and per
  metric, the first `MIMIC_METRICS_MAX_AUTHORITIES` distinct values are kept; later ones are
  counted under `authority="(other)"`. The two sentinels contain characters that no
  normalised authority has, so a real host named `other` or `invalid` keeps its own series,
  and they never take a budget slot. The set restarts empty with the process.
  `mimic_http_request_duration_seconds` observes the exchanges whose response head was read;
  `unframed` and `rejected` exchanges are counted in `mimic_http_requests_total` only.
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
  From F3 the `mimic` image also holds the static `mimic-trust` binary (§7.1).
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
| `MIMIC_SIDECAR_API_BODY_BUDGET` | `64Mi` | Push bodies being read at once, in bytes (same syntax; at least `MIMIC_SIDECAR_API_MAX_REPORT`, at most 4Gi). |
| `MIMIC_STORE_TTL` | `30d` | Retention of a deployment's data after its last push, and of endpoints and samples after they were last seen (§7.3); 10s–365d. |
| `MIMIC_STORE_MAX_ENDPOINTS` | `512` | Endpoints per direction and deployment in the store, 1–10000. |
| `MIMIC_STORE_MAX_PUSH_ENDPOINTS` | `256` | Endpoints per direction in one pushed report (§7.2), 1–10000, at most `MIMIC_STORE_MAX_ENDPOINTS`. |
| `MIMIC_STORE_MAX_LITERALS` | `32` | Literal limit of the store's templating (§6), 2–1000. |
| `MIMIC_STORE_SAMPLE_BUDGET` | `4Mi` | Sample bytes per deployment in the store (1Mi–1Gi). |
| `MIMIC_STORE_MAX_DEPLOYMENTS` | `50` | Deployments with data per namespace (§7.3), 1–100000. |
| `MIMIC_STATUS_INTERVAL` | `30s` | Period of the `MimicWorkload` status update (§7.4), 1s–1h. |
| `MIMIC_STATUS_POD_WINDOW` | `5m` | A pod counts in `status.discovery.pods` while its last push is this recent, 1s–24h. |
| `MIMIC_UI_ADDR` | `127.0.0.1:8080` | Read API (§7.5) and, from F6, UI listener; plain HTTP, loopback peers only. |

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
| `MIMIC_DISCOVERY_MAX_ENDPOINTS` | `256` | Endpoints kept per direction (§6), 1–10000. Not set by the webhook. |
| `MIMIC_DISCOVERY_MAX_LITERALS` | `32` | Distinct literal segments below a template node before it absorbs them into a parameter (§6), 2–1000. Not set by the webhook. |
| `MIMIC_DISCOVERY_SAMPLE_BUDGET` | `16Mi` | Bytes held by samples (`<n>`, `<n>Ki`, `<n>Mi`; 1Mi–1Gi). Not set by the webhook. |
| `MIMIC_DISCOVERY_CAPTURE_BUDGET` | `8Mi` | Bytes of bodies captured in flight and queued (same syntax; 1Mi–1Gi). Not set by the webhook. |
| `MIMIC_DISCOVERY_QUEUE` | `1024` | Exchange records waiting for the recorder (1–65536). Not set by the webhook. |
| `MIMIC_DISCOVERY_PUSH_INTERVAL` | `30s` | Period of the discovery push (§6, §7.2), 1s–1h. Not set by the webhook. |
| `MIMIC_DISCOVERY_REDACT_NAMES` | empty | Extra names redacted by name (§6), comma-separated, at most 64, each 1–64 characters of `a-z0-9-_` (compared like the built-in names); the webhook sets the sorted, deduplicated union, lower-cased, of `spec.discovery.redact` of every `MimicWorkload` naming the deployment. |

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
  pods, Deployments or other Secrets.
- The webhook never logs or stores the pod spec or the patch (env values may hold secrets).
- Images are distroless, pinned by digest, scanned in CI (no fixable HIGH/CRITICAL).
- Dependencies checked by `cargo deny` (advisories, licences, sources).
- No sensitive value reaches Valkey or a log without redaction (§6). Discovery keeps no
  opaque body, no hash of a redacted value, and never logs a header value, query, body or
  sample; the local report is served on loopback only. From F5 the controller re-applies the
  redaction to every pushed report before it reaches Valkey or a log (§7.3), never logs
  report content, and serves the stored reports on loopback only (§7.5). Valkey keeps only
  redacted reports, sequence numbers and pod names.
- TLS key custody (§5): the seed exists only in its Secret and the controller's memory; root
  keys only in the controller's memory; intermediate and leaf keys only in the sidecar's
  memory (non-dumpable, no core file). Tokens and keys are never logged.
- The sidecar API port is admitted by the controller NetworkPolicy from any source by
  default (injected pods can be in any namespace; `networkPolicy.sidecarApiFrom` narrows it).

### 11.1 Accepted limits

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
  API (and, from F8, to its recordings). Other namespaces are out of reach: the namespace
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
- Discovery is best effort under load: dropped records, a full sample budget or the endpoint
  limit make counters and samples partial (counted in §9 metrics), never slow traffic. A
  `404`/`405` reaching a path no other status has created is not recorded. Template
  absorption can merge distinct resources below one node (an API with more than
  `MIMIC_DISCOVERY_MAX_LITERALS` literal routes at one level, e.g. static files at the root).
- Emptying the seed Secret rotates every root: running sidecars then fail the chain check
  and pass everything through (`no_ca`) until their pod is recreated. The trust wiring and
  `spec.tls.exclude` apply at pod creation.

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
| FND-10 | CI runs as dependent stages: lint → unit and integration tests → image build and scan → kind e2e, plus a chart stage (`helm lint --strict`; the release-pinned chart renders both mimic images by digest, `scripts/check-release-chart.sh`; the pins of `images/Dockerfile` match `values.yaml` and the kind node image in ci.yml, `scripts/check-pins.sh`) and a supply-chain stage: `cargo deny` (advisories, licences, sources, and the bans of `deny.toml`, which deny the Brotli crates), `zizmor`, `actions/dependency-review-action` on a pull request, a dependency cooldown (`scripts/check-cooldown.py`: a crate that a pull request adds or bumps in a `Cargo.lock` must be published at least 14 days before, unless `scripts/cooldown-allowlist.toml` lists it with a reason), gitleaks on a pull request, `trivy config` on the rendered chart and on the Dockerfile (HIGH and CRITICAL fail), and CodeQL for Rust and GitHub Actions. The image job scans the mimic, mimic-init and Valkey images with Trivy. A weekly and a push-to-main OpenSSF Scorecard run reports its result without gating. A single `ci-ok` check sums up every job except Scorecard: it fails if any job fails or is cancelled, and accepts a skipped job only when the job's own path filter excluded it (the HTTP/1 codec fuzz job, PRX-28, on a change outside `crates/mimic-core/src/proxy/http1/`, `crates/mimic-core/src/proxy.rs`, the workspace `Cargo.toml` and `Cargo.lock`, and `fuzz/`; the discovery recorder fuzz job, DSC-23, on a change outside `crates/mimic-core/src/discovery/` and `fuzz/`). A `v*` tag is released only from a commit on `main`; it publishes multi-arch signed images with SBOM and provenance, and the signed Helm chart pinned to the digests of both mimic images. |

### 12.2 F1 — Injection

Levels: **U** unit in `mimic-core`, **I** integration in `crates/*/tests`, **E** kind e2e in
`scripts/e2e.sh`, **C** CI pipeline.

| ID | Criterion |
|---|---|
| INJ-01 | **U.** The selection decision follows the table of §7.1 for every combination of namespace label (`enabled`, `selected`, absent), pod label (`enabled`, `disabled`, absent) and presence of a `MimicWorkload` whose `spec.deployment` is the pod's deployment; the excluded namespaces are never injected. The deployment name is derived from the controlling ReplicaSet owner and the `pod-template-hash` label; any other owner gives `unsupported-owner`. |
| INJ-02 | **U.** A pod with `hostNetwork: true`, a container port 15001, 15006 or 15100, or `runAsUser: 1337` (pod or app container) is skipped with the matching reason; the patch only adds `mimic.platform.io/skipped: <reason>`. A pod that already has a `mimic-sidecar` init container gets an empty patch. |
| INJ-03 | **U.** The injection patch is a JSON Patch, byte-identical for the same input, that puts `mimic-init` then `mimic-sidecar` before the pod's own init containers (also when it has none), rewrites the app containers' `httpGet`, `tcpSocket` and `grpc` probes as in §4 and changes nothing else in them, and adds `mimic.platform.io/injected: <version>`. `mimic-init`: UID 0, `allowPrivilegeEscalation: false`, drop `ALL`, add only `NET_ADMIN`/`NET_RAW`, read-only root, `RuntimeDefault`. `mimic-sidecar`: `restartPolicy: Always`, UID/GID 1337, `runAsNonRoot`, no capability, read-only root, `RuntimeDefault`, startup probe `/readyz` and liveness probe `/healthz` on 15100, the configured resources and the §10.3 variables. Neither has a volume mount (amended by TLS-11). |
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
| DSC-05 | **U.** Redaction by name: each built-in name of §6, and names containing them in any case with `-`/`_` (`X-Api-Key`, `client_secret`, `refreshToken`, `Session-Id`) or glued into one token (`JSESSIONID`, `sessionid`, `csrftoken`, `xapikey`, `clientsecret`, `dbpassword`), while `company`, `author`, `panel` and `passenger` are kept, as a header, query key, form field and JSON key (also nested, and with an object or array value, which becomes the marker as a whole), gives `<redacted>`; `Authorization: Bearer x` → `Bearer <redacted>`, `Cookie: a=1; b=2` → `a=<redacted>; b=<redacted>`, `Set-Cookie: s=1; Path=/; HttpOnly` → `s=<redacted>; Path=/; HttpOnly`; a name of `MIMIC_DISCOVERY_REDACT_NAMES` is redacted; in a URL-valued header (`Location: https://h/cb?code=C&access_token=T`, `Referer`, `Link: <u?api_key=K>; rel=next`) the query parameters (and fragment) are redacted by name; `id`, `name` and `amount` are not; neither are `company`, `author`, `panel`; `tokens`, `auth.token`, `authToken` and `Authorization` are, extra names follow the same token rule, and `Link: <cb?token=T>` (relative URL) is redacted. The URL redactions are counted under `header` and only for the 64 kept headers. |
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
| CTL-01 | **U.** The structural check of §7.3 accepts every report the recorder produces within the §6 bounds, merged or not (`proptest`, ≥ 10 000 cases), and rejects, each with its own fixed `reason`: a version other than 1; 21 samples; 17 hosts or a host over 1024 bytes; status 99 or 600; a histogram bucket out of range; a schema of 513 nodes, of 17 levels or with a cut level of 17; a 129-byte name; 65 query keys or headers; 129 volatile fields or a 33-hash sketch; a 2049-byte or 33-segment path or template; 17 media types on a side; a 256 KiB + 1 sample body or a sample body for an opaque media type; an unknown field; more than `MIMIC_STORE_MAX_PUSH_ENDPOINTS` endpoints in a direction is `push_limit`, apart from the `400` reasons. `Report::from_json` then the check never panic on arbitrary input (`proptest`, and CTL-22). |
| CTL-02 | **U.** The bounded merge equals the §6 `merge` while the result is within the store limits, and below them does not depend on the order or grouping of pushes (`proptest`); two pushes each below the literal limit at a node absorb it when their union exceeds it; counters and buckets at 2^64 − 1 stay there; the result's `pod` is null. Anti-crowding, with `MIMIC_STORE_MAX_ENDPOINTS` = 4: a store of 4 endpoints with 100 exchanges each, merged with a push of 50 endpoints `/!0` … `/!49` of 1 exchange each, keeps the 4 (50 `endpoint_limit` refusals, no eviction); a push of an endpoint with 101 exchanges evicts the stored endpoint with the fewest exchanges, ties broken by key (`evicted` = 1); a new endpoint tying the lowest stored count does not displace it. Samples, with a full budget: a new `5xx` sample of an endpoint holding only `2xx` samples evicts samples of the (endpoint, class) with the most samples until it fits, never the only sample of a class; a new `2xx` sample of that endpoint is refused (`budget`); when room cannot be made without emptying a class, nothing is evicted. The same input always gives byte-identical output; `refused` and `evicted` counts are returned per kind and reason. |
| CTL-03 | **U.** Re-redaction is idempotent and leaves unchanged a report recorded with the same redactor. For reports recorded with a redactor that lacks a name the controller's redactor has (`proptest`, ≥ 10 000 cases), with a canary under that name at a random place (query, header, form or JSON body at any depth, request or response, URL-valued header), the re-redacted report contains neither the canary nor its XXH3 hash in decimal or hex, its sample keys are the hashes of the re-redacted content, and the field is marked `redacted` without sketch. A canary in a detected form (JWT, card, token prefix) in a report that bypassed the sidecar's redaction is removed the same way. |
| CTL-04 | **U.** Retention: stamping a push with day `D` sets `last_seen = D` on each endpoint and sample; merge keeps the larger value (commutative, associative); dropping with day `T` and a TTL of `n` days removes exactly the endpoints and samples with `last_seen < T − n` (counted per kind); a sidecar report without `last_seen` parses, and a sidecar never serialises it. |
| CTL-05 | **U.** The key names of §7.3 for namespace `shop` and deployment `pay.v2` are byte-exact; a namespace or deployment name that is not DNS-1123 is refused before any key is built. The sequence decision applies a push whose sequence is above the stored one of its pod and session, or that has none, and reports any other as a duplicate; a session that is not 32 lower-case hex characters and a sequence of 0 or above 2^53 are `invalid`. |
| CTL-06 | **U.** The status of §7.4 is a pure function of `:meta`, the pods' push times, the window and the current time (passed in): byte-identical for the same input; a pod counts exactly when its last push is at or after now − window; without data the counts are zero and `lastPushTime` is absent; `lastPushTime` is RFC 3339 UTC in seconds; directions without endpoints are omitted. |
| CTL-07 | **I.** `POST /v1/workloads/<deployment>/report` answers as §7.2 says: `200` `{"applied": true}` and the stored report equals the bounded merge; the same body again, and a lower sequence of the same session, `200` `{"applied": false}` with `:report` and `:meta` unchanged; a new session of the same pod applies from sequence 1; `401` and `403` as TLS-09; `400` for malformed JSON, a bad session or sequence, a missing `Content-Length` and one CTL-01 violation; `405`; `413` when `Content-Length` is over `MIMIC_SIDECAR_API_MAX_REPORT`, received by the client before it has sent any body byte; `507` for a report over `MIMIC_STORE_MAX_PUSH_ENDPOINTS` (= 3 in the test) endpoints in a direction, with nothing stored, and for a new deployment once `MIMIC_STORE_MAX_DEPLOYMENTS` (= 2 in the test) deployments of the namespace have data, while those two still apply; `503` with `Retry-After` while Valkey is stopped, when the `TokenReview` fails, and when two concurrent bodies exceed `MIMIC_SIDECAR_API_BODY_BUDGET`. An unauthenticated request declaring a 1 GiB body is answered `401` while the client has sent only its headers. |
| CTL-08 | **I.** Rate limit and review cache: with `MIMIC_SIDECAR_API_RATE=5`, 30 requests sent at once from one source get 10 answers other than `429` and 20 `429` with `Retry-After: 1`, the fake API server receives no `TokenReview` for the `429`s, a second source is served meanwhile, and 1 s later 5 more requests from the first pass. Two pushes with the same token within 60 s cause one `TokenReview`; another token causes its own; a failed review is not cached (the next request with that token is reviewed again). |
| CTL-09 | **I.** Applied once across replicas: two controller processes share one Valkey; 16 simulated sidecars (pods of 2 deployments) each post 50 generated pushes to random replicas, 30 % of them posted twice and 10 % posted to both replicas at once, with limits that are not reached. For each deployment the stored `:report` is byte-identical to the bounded merge of its distinct pushes, `:meta` `exchanges` is their sum, and `mimic_store_writes_total{result="conflict"}` is above 0 on a replica (contention happened). Passes 3 runs in a row. |
| CTL-10 | **I.** Keys and expiry: after a push exactly the keys of §7.3 exist for its deployment, with their types and fields; with `MIMIC_STORE_TTL=10s` every key and both index members have an expiry of at most 10 s, refreshed by each applied push (a duplicate refreshes only its `:pods` field); 10 s (+ 2 s) after the last push the deployment's keys and index members are gone, and the `:pods` and `:seq` fields expire after min(24 h, TTL). |
| CTL-11 | **I.** Nothing sensitive reaches Valkey: pushes carrying the DSC-15 canaries unredacted (as a faulty sidecar would send them) and a canary under a name that only the `MimicWorkload`'s `spec.discovery.redact` lists are applied, and no value of any `mimic:v1:*` key (dumped with `SCAN` and `GET`/`HGETALL`/`ZRANGE`) contains a canary, its XXH3 hash or the bearer token; nor does stdout or `/metrics`. |
| CTL-12 | **I.** Each push writes one `sidecar_api` line with the §7.2 fields (`endpoint` `/v1/workloads/{deployment}/report`, `session`, `sequence`, `bytes`, `result`, `reason` for `invalid`), at DEBUG for `rate_limited`; after a known sequence of pushes `mimic_sidecar_api_requests_total{endpoint,result}`, `mimic_store_pushes_total`, `mimic_store_writes_total`, `mimic_store_refused_total`, `mimic_store_evicted_total`, `mimic_store_report_bytes` and `mimic_store_write_duration_seconds` have the expected values; one WARN `store limit reached` per deployment and reason and one WARN `store evicted` per deployment and kind; no line and no series contains a path, template, header, sample, token or report content. |
| CTL-13 | **I.** Read API: `GET /v1/workloads`, `/v1/workloads/<ns>/<dep>` and `/v1/workloads/<ns>/<dep>/report` on `MIMIC_UI_ADDR` answer as §7.5 (list sorted, pods map, report bytes equal to `:report`); `404` for a deployment without data, `400` for an invalid name, `405` for `POST` and `DELETE`, `503` while Valkey is stopped, `500` with one ERROR `stored report unreadable` for a corrupted `:report` (also refused by a push, `503`, without overwriting it). With `MIMIC_UI_ADDR=0.0.0.0:<port>` a request from the host's non-loopback address gets `403`. |
| CTL-14 | **I.** An invalid value of each variable added in §10.2 (out of range, bad suffix, `MIMIC_SIDECAR_API_BODY_BUDGET` below `MIMIC_SIDECAR_API_MAX_REPORT`, `MIMIC_STORE_MAX_PUSH_ENDPOINTS` above `MIMIC_STORE_MAX_ENDPOINTS`, an unparsable `MIMIC_UI_ADDR`) and of `MIMIC_DISCOVERY_PUSH_INTERVAL` exits 2 before listening (FND-01); `1m`, `2h` and `30d` parse as durations. |
| CTL-15 | **I.** Shipping: with `MIMIC_DISCOVERY_PUSH_INTERVAL=1s` and exchanges to the test app, the sidecar posts to `/v1/workloads/<MIMIC_WORKLOAD>/report` bodies with one 32-hex session and sequences 1, 2, …; the exchanges of all posts sum to the exchanges recorded; an interval without exchanges posts nothing; the token file is read for each post; after a `200`, `/discovery` holds only exchanges recorded since; a restarted sidecar posts a new session from sequence 1. Without `MIMIC_CONTROLLER_URL` nothing is posted and one INFO `report shipping disabled` is written. |
| CTL-16 | **I.** Retries: while the fake API answers `503`, resets the connection or stays silent past 30 s, the sidecar posts the same bytes with the same sequence again with the 1, 2, 4 … s backoff, and the exchanges recorded meanwhile go into the next sequence, posted only after the pending one is acknowledged; `429` with `Retry-After: 3` delays the retry by at least 3 s; `413` is followed by the same sequence without samples; `400` drops the pending report with one WARN `report rejected` and the next sequence follows. Samples of the pending and current reports never exceed `MIMIC_DISCOVERY_SAMPLE_BUDGET`. `mimic_discovery_pushes_total{result}` and `mimic_discovery_last_push_timestamp_seconds` follow; one WARN `report push failed` per failure streak and one INFO `report push recovered`; no line holds the token. |
| CTL-17 | **I.** At `SIGTERM`, a sidecar with a pending report and unshipped exchanges posts the pending then the current report and exits 0 within `MIMIC_SHUTDOWN_GRACE`; the fake API received every exchange once. With the fake API silent, it exits within the grace (+ 1 s) with one WARN `report not shipped` whose `exchanges` is the number lost. |
| CTL-18 | **I.** Status: within `MIMIC_STATUS_INTERVAL` (+ 1 s) of a push, every `MimicWorkload` naming that deployment gets the §7.4 `status.discovery` through the status subresource by server-side apply with field manager `mimic-controller`; an unchanged status sends no request; a `MimicWorkload` of a deployment without data gets the empty status; while Valkey is stopped no status request is sent and one WARN `status not updated` is written; `mimic_status_updates_total` counts the applies. |
| CTL-19 | **E.** The CRD has the status subresource and schema of §7.4 and the printer columns `Mode`, `Endpoints`, `Pods`, `Last push`; `kubectl auth can-i` as the controller is `yes` for `patch mimicworkloads/status` and `no` for `update mimicworkloads`, `get pods` and `list replicasets`; Valkey answers `CONFIG GET maxmemory` with 75 % of its memory limit and `maxmemory-policy` `noeviction`; no Service and no NetworkPolicy port exposes `MIMIC_UI_ADDR`. |
| CTL-20 | **E.** On kind with the shared apps: `users` (B, 2 replicas) in an `enabled` namespace with a `MimicWorkload`, `front` (A) calling it, and the client sending 40 `GET /front/users/<id>` (numeric ids, a canary bearer token) and 5 `PUT` with a canary in the JSON body, spread over both pods. Within 2 push intervals, through `kubectl port-forward` to a controller pod's `MIMIC_UI_ADDR`, `/v1/workloads/<ns>/users/report` has `GET /users/{users_id}` with as many `200`s as the client got for its `GET`s (probe requests are not recorded) and `PUT /users/{users_id}` with 5, with response schemas and samples; `/v1/workloads/<ns>/users` lists both pods; within `MIMIC_STATUS_INTERVAL` more the `MimicWorkload` shows `endpoints.inbound: 2`, `pods: 2` and a `lastPushTime`, also in `kubectl get mimicworkload`. No canary is in the report, in any Valkey value (`valkey-cli` dump), in the controller's logs or in the pods' `/discovery`. |
| CTL-21 | **E.** Churn: with traffic from the client before, during and after `kubectl rollout restart deployment/users`, and one controller pod deleted while pushes are flowing, the merged `200` count of `GET /users/{users_id}` equals the number of `200`s the client received (no exchange lost at `SIGTERM`, none counted twice); with `MIMIC_STATUS_POD_WINDOW=30s` (set by the e2e), `status.discovery.pods` comes back to 2 after the old pods' window, and the endpoints are unchanged. |
| CTL-22 | **C.** The DSC-23 fuzz target also feeds arbitrary bytes to `Report::from_json`, the structural check, re-redaction and the bounded merge with a fixed valid report: no panic, and every merge result passes the check. Same CI schedule and `ci-ok` path-filter rule as DSC-23. |

## 13. Test strategy

- **Unit** tests in `mimic-core` (pure logic) and next to the code.
- **Integration** tests per crate in `tests/`: in-process routers, or the real binary started
  as a child process. From F5, controller store tests use a real Valkey (§12.6); the fake RESP
  server of FND-06 stays for readiness only.
- **End-to-end** on kind with the Helm chart (`scripts/e2e.sh`), locally and in CI.
- A criterion is covered when a test whose name contains its ID (lower case) passes.
- A timing bound ("within 1 s") is measured from the event that triggers it (the close, the
  signal), not from the start of the test, and the test's own wait for that event has its own
  larger timeout, so that a slow runner fails only when the sidecar is really late.
