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
- Excluded inbound ports: the ports of the `tcpSocket` and `grpc` probes of the app containers
  (named ports resolved), which the sidecar would otherwise answer for an app that is down, and
  the sidecar admin port 15100. The webhook reads them from the pod spec. API traffic on an
  `httpGet` probe port is captured.
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
  decimal number, a request `transfer-encoding` whose last coding is not `chunked` or that
  comes with HTTP/1.0, or an invalid chunk size. The exchange then writes its access line at
  once (`result: unframed`, with the fields known so far), and the rest of the connection is
  relayed as an opaque tunnel, with no further exchange line.
- **Upgrades**: a request answered with `101` (WebSocket, `h2c` upgrade), or a `CONNECT`
  answered with 2xx, continues as an opaque tunnel after the response. After a request that
  asks for an upgrade or is a `CONNECT`, the relay reads no more client bytes until the final
  response to it arrives (the client must wait for it, RFC 9110 §7.8, §9.3.6). A declined
  upgrade leaves a plain HTTP/1 connection.
- **Failures**: the sidecar never synthesises a response. When the destination refuses the
  connection, the client connection is closed without any byte; when it closes or resets
  before answering, the client sees the same close or reset (HTTP/1), or the stream is reset
  (HTTP/2). An HTTP/2 client that breaks the protocol at connection level gets `GOAWAY` (the
  `h2` stack) and one `rejected` access line.
- **Idle and long-lived connections**: no idle, header-read, handshake or request timeout, no
  cap on connections or streams. A connection lives until one side closes it. The HTTP/2
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
  `connections` (how many it closed), and exits 0.
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
- An HTTP/1 request whose first bytes arrive while `SIGTERM` closes its idle connection is
  not forwarded: the client sees the connection closed without a response and no access line
  is written. The app containers have already stopped then, so the client would get no
  answer without mimic either.
- WebSocket over HTTP/2 (extended `CONNECT`, RFC 8441) is not advertised, so clients use
  HTTP/1.1 for it.
- Inbound HTTPS, and outbound HTTPS that §5 passes through, are seen at connection level only
  (SNI, bytes, duration).
- Until F4, the path in access logs is not redacted: a secret placed in a path segment is
  logged.
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
  | the client aborts its handshake after receiving the sidecar's certificate (alert, close or reset), or the handshake with the client fails | `client_rejected` | yes |
  | otherwise | `intercepted` | — |

  The sidecar handshakes with the destination **before** answering the client: it offers the
  client's SNI and the client's ALPN list restricted to `h2` and `http/1.1` (client order),
  verifies the destination's chain against its upstream roots (below) and the SNI (the
  destination IP when there is no SNI), then completes the client's handshake with the ALPN
  the destination chose (none if it chose none). The decrypted client stream then goes through
  the same detection and the same relays as cleartext (§4.1): HTTP/1 through the byte-exact
  `http1` codec, HTTP/2 through the `h2` relay, anything else as an opaque decrypted tunnel.
- **Pass-through** means what §4.1 does for TLS today: a new TCP connection to the original
  destination receives the client's bytes unchanged, from the original ClientHello on, and the
  client sees the destination's own certificate. For `upstream_untrusted` and
  `client_cert_requested` the connection being handled is passed through at once (the
  sidecar's own TCP connection to the destination is closed); for `client_rejected` the
  connection being handled is lost (the client gave up on it) and the next one passes through.
  An I/O failure (reset, close) of the destination during the sidecar's handshake reaches the
  client as that reset or close, and nothing is remembered.
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
  - Without a valid intermediate (none yet, expired, mismatch) every outbound TLS connection
    is passed through (`no_ca`). The sidecar's readiness does not depend on the controller.
  - No key, seed or token is ever logged, written to disk, sent to Valkey or exposed on an
    endpoint. The sidecar disables core dumps (`RLIMIT_CORE` 0) and makes itself non-dumpable
    (`PR_SET_DUMPABLE` 0) before it generates a key.
- **Upstream roots** (what the sidecar verifies destinations against): the system store of the
  mimic image, plus every `image-ca.pem` written by `mimic-trust` for the pod's app images
  (§7.1), loaded at the first interception; the mimic roots are never in them. A destination
  whose certificate the app's own images trust is therefore verified, and a destination signed
  by a CA that mimic does not know is passed through, so that the app judges it itself.
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
  `MimicWorkload` naming the deployment, in `MIMIC_TLS_EXCLUDE`; a change applies to pods
  created afterwards (`kubectl rollout restart`), like the injection itself.
- **Mock mode (F10)**: the sidecar answers intercepted connections without connecting to the
  destination (it issues the leaf for the SNI and picks `h2` when offered, else `http/1.1`),
  and a connection that the table above would pass through is closed instead, without any
  byte towards the destination (the real destination is never called, §3).

## 6. Discovery

- An *endpoint* is identified by direction, authority (host), method and path template
  (`/users/{id}`). Query keys, header names and body schemas are inferred and merged.
- Per endpoint, the sidecar keeps: request/response counters by status, a latency histogram,
  and up to **20 samples** (each distinct status code and each distinct response shape first),
  bodies capped at **256 KiB**. Samples expire after **30 days** in Valkey.
- Volatile fields (values that change on every call: idempotency keys, timestamps, ids) are
  detected and marked; mock matching ignores them.
- **Redaction** happens in the sidecar, before anything leaves process memory: secrets in
  headers, query, path segments and bodies are replaced by stable markers.
- Under overload the sidecar stops recording (and counts the dropped exchanges) but keeps
  forwarding traffic.

## 7. Controller and storage

- `MimicWorkload` (namespaced CRD, `mimic.platform.io/v1alpha1`): selects the deployment named
  by `spec.deployment` (required, DNS-1123 name, immutable) in its namespace, holds
  `spec.mode` (`discovery` | `mock`, default `discovery`), `spec.tls.exclude` (F3, §5),
  latency settings; `status` reports pods, mode convergence and
  counts. F1 introduces the CRD with `spec.deployment` and `spec.mode` only; other fields arrive
  with their feature. When several `MimicWorkload`s name the same deployment, any of them
  selects it for injection; which one sets the mode is decided in F10.
- Sidecars push their local recordings periodically and pull the deployment's mode and, in mock
  mode, its aggregated egress recordings. Valkey keys are namespaced by
  `<namespace>/<deployment>`.
- The controller merges endpoints of all pods of a deployment (same endpoint key → merged
  counters, histograms, schemas, sample sets).
- The UI and its API are read-only and served on the controller's HTTP port, reached by
  `kubectl port-forward` (Kubernetes RBAC is the authentication).

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
  ReplicaSet is read. F5 extends this API with the same authentication.
- `POST /v1/tls/intermediate`, body `{"deployment": "<name>", "public_key": "<base64 DER
  SubjectPublicKeyInfo, P-256>"}` (at most 64 KiB) → `200` `{"certificate": "<PEM>"}`, the
  intermediate of §5 for that key signed by the deployment's root. `401` when the token is
  missing, invalid, expired or for another audience; `403` when the pod name does not match
  the deployment or has no bound-pod claim; `400` for a malformed body or key; `413` over the
  limit; `405` for another method; `503` when the seed is not loaded or the `TokenReview`
  fails.
- One log line per request (`target` = `sidecar_api`): `endpoint`, `namespace`, `pod`,
  `deployment`, `result` (`ok`, `unauthenticated`, `forbidden`, `invalid`, `unavailable`);
  never the token or the key. Metric `mimic_sidecar_api_requests_total{endpoint,result}`.

## 8. Mock

- Matching, first hit wins: (1) exact: method, authority, path, query, normalised body;
  (2) same endpoint, closest body ignoring volatile fields; (3) most frequent response of the
  endpoint. The response carries `x-mimic-match: exact|endpoint|fallback`.
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

  An intercepted connection is counted in `mimic_proxy_connections_total` and
  `mimic_proxy_bytes_total` under its decrypted protocol (`http1`, `h2`, or `tcp` for an
  opaque decrypted tunnel), with the SNI as `authority` when present; a passed-through one
  under `tls` as before. The controller exposes `mimic_sidecar_api_requests_total` (§7.2) and
  `mimic_tls_seed_loaded` (gauge, 1 once the seed is loaded).

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

### 10.3 Sidecar variables

Set by the webhook; the defaults are the values it sets.

| Variable | Default | Meaning |
|---|---|---|
| `MIMIC_INBOUND_ADDRS` | `0.0.0.0:15006,[::]:15006` | Inbound listeners. |
| `MIMIC_OUTBOUND_ADDRS` | `127.0.0.1:15001,[::1]:15001` | Outbound listeners, loopback only. |
| `MIMIC_WORKLOAD` | required | Deployment name. |
| `MIMIC_POD_NAMESPACE`, `MIMIC_POD_NAME`, `MIMIC_POD_IP` | required | Downward API. |
| `MIMIC_APP_PROBES` | `{}` | Original `httpGet` probes, JSON object keyed by `<container>/<livez\|readyz\|startupz>`. |
| `MIMIC_METRICS_MAX_AUTHORITIES` | `50` | Distinct `authority` label values kept per direction and metric (§9), 1–10000. Not set by the webhook in F2. |
| `MIMIC_TLS_CA` | empty | Deployment root certificate (PEM). Empty: no interception, every outbound TLS connection is passed through (`no_ca`), with one INFO line at start. |
| `MIMIC_TLS_EXCLUDE` | empty | Comma-separated exclusions (§5); an invalid entry or more than 256 exits with code 2. |
| `MIMIC_CONTROLLER_URL` | required when `MIMIC_TLS_CA` is set | Sidecar API base URL (`https://…`). |
| `MIMIC_CONTROLLER_CA` | required when `MIMIC_TLS_CA` is set | CA (PEM) the sidecar API certificate is verified against. |
| `MIMIC_TLS_TOKEN_FILE` | `/var/run/secrets/mimic.platform.io/token` | Projected token, re-read for every sidecar API request. |
| `MIMIC_TRUST_DIR` | `/mimic` | `mimic-trust` volume: binary and fallback store written at start, `trust/*/image-ca.pem` read as upstream roots. |

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
- The controller's RBAC is minimal: `get`/`list`/`watch` on `mimicworkloads`; `get`/`list`/`watch`/`patch`
  on its own `MutatingWebhookConfiguration` only (by name, and restricted to `caBundle` by a
  `ValidatingAdmissionPolicy`); `get`/`update` on its webhook Secret and its TLS seed Secret only (by name, in its
  namespace); `create` on `tokenreviews` (sidecar API authentication, §7.2). No access to
  pods, Deployments or other Secrets.
- The webhook never logs or stores the pod spec or the patch (env values may hold secrets).
- Images are distroless, pinned by digest, scanned in CI (no fixable HIGH/CRITICAL).
- Dependencies checked by `cargo deny` (advisories, licences, sources).
- No sensitive value reaches Valkey or a log without redaction (§6).
- TLS key custody (§5): the seed exists only in its Secret and the controller's memory; root
  keys only in the controller's memory; intermediate and leaf keys only in the sidecar's
  memory (non-dumpable, no core file). Tokens and keys are never logged.
- The sidecar API port is admitted by the controller NetworkPolicy from any source by
  default (injected pods can be in any namespace; `networkPolicy.sidecarApiFrom` narrows it).

### 11.1 Accepted limits

- Capture is an observability and test tool, not a security boundary: an app container with
  `NET_ADMIN`, or whose image runs as UID 1337 without declaring it in the pod spec, escapes it.
- All traffic to a `tcpSocket` or `grpc` probe port bypasses the sidecar (not only probes): an
  API served on such a port is not captured. `exec` probes are left as they are.
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
  same namespace. Sidecar identity is bound to the real owner when sidecars authenticate (F5).
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
| FND-10 | CI runs as dependent stages: lint → unit and integration tests → image build and scan → kind e2e, plus a security stage (`cargo deny`, `zizmor`); a single `ci-ok` check sums them up: it fails if any job fails or is cancelled, and accepts a skipped job only when the job's own path filter excluded it (the HTTP/1 codec fuzz job, PRX-28, on a change outside `crates/mimic-core/src/proxy/http1/` and `fuzz/`). A `v*` tag publishes multi-arch signed images with SBOM and provenance, and the signed Helm chart. |

### 12.2 F1 — Injection

Levels: **U** unit in `mimic-core`, **I** integration in `crates/*/tests`, **E** kind e2e in
`scripts/e2e.sh`, **C** CI pipeline.

| ID | Criterion |
|---|---|
| INJ-01 | **U.** The selection decision follows the table of §7.1 for every combination of namespace label (`enabled`, `selected`, absent), pod label (`enabled`, `disabled`, absent) and presence of a `MimicWorkload` whose `spec.deployment` is the pod's deployment; the excluded namespaces are never injected. The deployment name is derived from the controlling ReplicaSet owner and the `pod-template-hash` label; any other owner gives `unsupported-owner`. |
| INJ-02 | **U.** A pod with `hostNetwork: true`, a container port 15001, 15006 or 15100, or `runAsUser: 1337` (pod or app container) is skipped with the matching reason; the patch only adds `mimic.platform.io/skipped: <reason>`. A pod that already has a `mimic-sidecar` init container gets an empty patch. |
| INJ-03 | **U.** The injection patch is a JSON Patch, byte-identical for the same input, that puts `mimic-init` then `mimic-sidecar` before the pod's own init containers (also when it has none), rewrites the app containers' `httpGet` probes as in §4 and changes nothing else in them, and adds `mimic.platform.io/injected: <version>`. `mimic-init`: UID 0, `allowPrivilegeEscalation: false`, drop `ALL`, add only `NET_ADMIN`/`NET_RAW`, read-only root, `RuntimeDefault`. `mimic-sidecar`: `restartPolicy: Always`, UID/GID 1337, `runAsNonRoot`, no capability, read-only root, `RuntimeDefault`, startup probe `/readyz` and liveness probe `/healthz` on 15100, the configured resources and the §10.3 variables. Neither has a volume mount (amended by TLS-11). |
| INJ-04 | **U.** `MIMIC_EXCLUDE_INBOUND_PORTS` given to `mimic-init` holds, sorted and deduplicated, the ports of the `tcpSocket` and `grpc` liveness/readiness/startup probes of the app containers (named ports resolved against the container's ports, unresolvable names ignored) and 15100; `httpGet` probe ports are not in it. `MIMIC_APP_PROBES` holds every original `httpGet` probe under its `<container>/<kind>` key, with the named port resolved and the probe's `timeoutSeconds`; an `httpGet` probe whose named port does not resolve is left unchanged and absent from `MIMIC_APP_PROBES` (it fails as it would without mimic). Each rewritten probe points to `/app-health/<container>/<kind>` on 15100 with the original timings and thresholds. |
| INJ-05 | **U.** The `iptables-restore` input generated for IPv4 and for IPv6 is deterministic and: redirects inbound TCP to a local destination (`addrtype LOCAL`) to the inbound port except the excluded ports; returns outbound traffic of the proxy UID and on `lo`; redirects other outbound TCP to the outbound port; rejects outbound UDP to port 443 in `filter`; uses only mimic-owned chains (`MIMIC_INBOUND`, `MIMIC_OUTPUT`) plus one jump from each built-in chain it hooks, which it flushes first (§4). |
| INJ-06 | **I.** `mimic-init` pipes the rules to `iptables-restore` and `ip6tables-restore` found on `PATH`, logs one JSON line `redirection installed` with the families and excluded ports, and exits 0; it skips IPv6 with an INFO log when the pod has no IPv6; it exits 1 with an ERROR log naming the family when a restore command fails or is missing, and 2 on an invalid variable (FND-01). It never logs other content of its environment. |
| INJ-07 | **I.** The sidecar binds its inbound and outbound listeners from §10.3 before `/readyz` answers 200; the outbound listeners accept connections on loopback only. A connection without an original destination, or whose original destination is a sidecar listener, is closed and counted in `mimic_tcp_connections_total{direction,result}` (`no_original_dst` or `loop`, PRX-10); tunnelled connections are counted with `result="ok"` or `result="connect_error"`. |
| INJ-08 | **I.** The webhook answers only HTTPS on `MIMIC_WEBHOOK_ADDR`. `POST /inject/namespace`, `/inject/pod` and `/inject/workload` with an `AdmissionReview` v1 return an `AdmissionReview` with the same `uid`, `allowed: true` and a base64 `JSONPatch` (or none when the patch is empty). A malformed body gets 400, a body over 4 MiB gets 413, another method gets 405, and an internal error gets `allowed: false` with a message (fail-closed). |
| INJ-09 | **I.** Each admission writes one log line (`target` = `webhook`) with namespace, pod name or `generateName`, deployment, `result` (`injected`, `skipped`, `error`) and `reason`, and increments `mimic_webhook_admissions_total{result,reason}`. Neither the logs nor the metrics contain any env value, annotation value or other pod spec content (checked with a canary env value). |
| INJ-10 | **U.** The certificate decision is a pure function of the stored certificates and the current time (passed in): no or unparsable CA → new CA and serving certificate; serving certificate missing, expiring within 30 days or with the wrong SAN → new serving certificate signed by the existing CA; otherwise keep. |
| INJ-11 | **E.** After install, the webhook Secret holds a CA and a serving certificate for `<service>.<namespace>.svc`, the `caBundle` equals the CA, and `mimic_webhook_certificate_expiry_timestamp_seconds` is exposed. With 2 controller replicas both serve the same certificate; after a restart and a `helm upgrade` the certificate and `caBundle` are unchanged. The controller is unready until its CA is in every webhook's `caBundle`, the certificate is loaded (in that order) and the `MimicWorkload` cache is synced. |
| INJ-12 | **E.** The chart installs the `MimicWorkload` CRD (an invalid `spec.mode` or a missing or invalid `spec.deployment` is rejected, a change of `spec.deployment` is rejected, `spec.mode` defaults to `discovery`), a `MutatingWebhookConfiguration` with the three webhooks of §7.1 (`failurePolicy: Fail`, `sideEffects: None`, `reinvocationPolicy: IfNeeded`, pod `CREATE` only, excluded namespaces), and the RBAC of §11: `kubectl auth can-i` as the controller service account is `yes` exactly for the listed verbs and `no` for creating Secrets, reading other Secrets, reading pods and patching another webhook configuration. Patching anything but `caBundle` of its own configuration as that account is denied by the `ValidatingAdmissionPolicy`. |
| INJ-13 | **E.** In a namespace labelled `enabled` (Pod Security `privileged`), a Deployment with an HTTP readiness and liveness probe on its API port, a `tcpSocket` probe on a second port and an init container that opens a TCP connection to a Service rolls out Ready: `mimic-init` terminated with exit code 0, `mimic-sidecar` running, the injected annotation present, no restart. A request from another pod to the app Service succeeds and increments the sidecar's `mimic_tcp_connections_total{direction="inbound",result="ok"}`; a request from the app container to another Service succeeds and increments `{direction="outbound",result="ok"}`. The pod's readiness probe points to `/app-health/…` on 15100. When the app's readiness path starts failing, the pod becomes NotReady, and Ready again when the path recovers. |
| INJ-18 | **I.** The sidecar serves `GET /app-health/<container>/<kind>` on its admin port only for the probes in `MIMIC_APP_PROBES`, and 404 for any other path under `/app-health/`. It forwards the original method, path, port and headers and answers the app's status code (2xx, 3xx without following, 4xx and 5xx unchanged). It answers 503 when the app refuses the connection or exceeds the probe's `timeoutSeconds`, and talks HTTPS without verifying the certificate when the probe's scheme is HTTPS. An invalid `MIMIC_APP_PROBES` exits with code 2 (FND-01). |
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
| TLS-07 | **U** (controller). The same seed and `<namespace>/<deployment>` give the same root key and subject; another deployment, namespace or seed gives another key. The root is `CA:true`, `keyCertSign`, ECDSA P-256, valid from seed creation − 1 h for 10 years. An intermediate issued for a P-256 public key verifies under the root with `pathLen:0`, `CA:true`, `keyCertSign`, valid from issuance − 1 h for 25 h (time passed in). |
| TLS-08 | **I** (controller, fake API server). With the seed Secret empty, the controller writes a 32-byte `seed` and its `created` time and becomes ready only after (and after the INJ-11 conditions), with `mimic_tls_seed_loaded 1`; two replicas started together end with one seed; a restart keeps it; after the Secret's seed is replaced, intermediates are signed by the new root within 60 s (+ 5 s). The seed value appears in no log line. |
| TLS-09 | **I** (controller, fake API server answering `TokenReview`). `POST /v1/tls/intermediate` answers as §7.2 says: `200` with an intermediate for the posted key that chains to the root of (token namespace, requested deployment); `401` without token, with an unauthenticated token and with a token for another audience (the `TokenReview` is sent with `audiences: [mimic.platform.io]`); `403` for a pod name of another deployment and without bound-pod claim; `400`, `413`, `405`; `503` when the `TokenReview` call fails. The listener applies the INJ-19 limits with its own budget. |
| TLS-10 | **I** (controller). Each sidecar API request writes one `sidecar_api` line with the §7.2 fields and increments `mimic_sidecar_api_requests_total{endpoint,result}`; a canary token and the posted key appear in no log line and no metric. |
| TLS-11 | **U.** The injection patch adds what §7.1 "TLS trust wiring" lists, byte-identical for the same input: the two volumes (token audience, expiry, path), the sidecar mounts and variables, one `mimic-trust-<n>` per distinct app image in first-appearance order with its image, pull policy, command, security context, resources, mount and variables; in every app container and app init container the read-only `subPath` mount of its image and the five variables, except those its `env` defines; a literal `JAVA_TOOL_OPTIONS` gets mimic's options prepended, a `valueFrom` one is untouched. No mimic container mounts an app volume; app containers get no `mimic-token` mount. The new `name-conflict` cases skip the pod. |
| TLS-12 | **U.** `MIMIC_TLS_EXCLUDE` is the sorted, deduplicated union of `spec.tls.exclude` of every `MimicWorkload` naming the deployment (absent when none); `MIMIC_TLS_CA` is the deployment's root, `MIMIC_CONTROLLER_URL` and `MIMIC_CONTROLLER_CA` as §7.1 says. **I.** An admission while the seed is not loaded gets `allowed: false`. |
| TLS-13 | **I** (`mimic-trust`, temporary root directory). With a Debian-style store, a RHEL-style store, an image whose own `SSL_CERT_FILE` and `NODE_EXTRA_CA_CERTS` name extra files, and an image without store, it writes the five files of §7.1 with the documented contents and order, mode `0644`, duplicates removed, unparsable blocks skipped and counted; the PKCS12 file lists every bundle certificate as a trusted entry readable with password `mimic` (read back with the `p12-keystore` crate). It logs one JSON `trust bundle written` line (WARN `no image trust store` in the last case), exits 0; 1 when the output is not writable; 2 when `MIMIC_TRUST_CA` is missing or not a certificate. |
| TLS-14 | **C.** `mimic-trust` in the `mimic` image (amd64 and arm64) is statically linked (no `PT_INTERP`, no `DT_NEEDED`); `scripts/check-image.sh` runs it in a `busybox:musl` container and in an empty (`scratch`) image with a store copied in, and both exit 0 with a bundle. |
| TLS-15 | **I** (sidecar, fake sidecar API). With `MIMIC_TLS_CA` set, the sidecar is ready before it has an intermediate; it reads the token file for each request (a changed file sends the new token), posts its public key, and starts intercepting once it holds an intermediate that chains to `MIMIC_TLS_CA` (`mimic_tls_intermediate_expiry_timestamp_seconds` set); before that, and when the API fails, outbound TLS is passed through (`no_ca`) and failures are retried with the TLS-05 backoff (`result="error"`). An intermediate from another root is refused: ERROR `tls ca mismatch`, `result="mismatch"`, no interception. A 4 s intermediate is renewed about 2 s after issuance. A canary token appears in no log line, and no line contains `PRIVATE KEY`. A missing `MIMIC_CONTROLLER_URL` with `MIMIC_TLS_CA` set exits with code 2. |
| TLS-16 | **I.** The sidecar's `/proc/<pid>/limits` shows a core file size of 0 and its `/proc/<pid>/environ` is owned by root (non-dumpable), once it has started. |
| TLS-17 | **I.** Interception, outbound: a client that trusts `MIMIC_TLS_CA` connects with SNI `api.test` to a destination whose certificate for `api.test` comes from a CA in a `trust/0/image-ca.pem`; the client sees a leaf for `api.test` issued by the intermediate, within its validity, and the destination sees SNI `api.test`. Over it, the PRX-15 corpus keeps its byte equality on the decrypted streams; with ALPN `h2` (offered `h2,http/1.1`, destination supporting `h2`) the PRX-04 concurrent streams and unary gRPC pass unchanged and the client's negotiated ALPN equals the destination's choice; a client without ALPN gets HTTP/1.1. Exchange lines have `tls: true`, and `Host: api.test:443` gives `authority` `api.test`. A ClientHello without SNI to `127.0.0.1` gets a leaf with that IP SAN. Two connections with the same SNI get the same leaf until the intermediate changes. |
| TLS-18 | **I.** A destination whose certificate is self-signed, expired, for another name, or that only speaks TLS 1.1 is passed through: the destination receives the client's ClientHello byte for byte, the client sees the destination's own certificate (or its own failure, as without mimic); one `connection` line `protocol: tls`, `interception: upstream_untrusted`; one WARN `tls passthrough` per destination; the next connection is passed through without the sidecar's own handshake (the destination accepts one handshake from the sidecar in total); `mimic_tls_passthrough_destinations` counts it. A destination that resets during the sidecar's handshake resets the client within 1 s and is not remembered. |
| TLS-19 | **I.** A destination that requests a client certificate is passed through (`client_cert_requested`): a client presenting its certificate completes mTLS with the destination, which sees that certificate; it is remembered. |
| TLS-20 | **I.** A client that trusts only the destination's CA (not `MIMIC_TLS_CA`) fails its first handshake (it rejects the mimic leaf), the destination is remembered as `client_rejected` with one WARN `tls passthrough`, and the client's next connection succeeds with the destination's own certificate. |
| TLS-21 | **I.** With `MIMIC_TLS_EXCLUDE=*.excluded.test`, a connection with SNI `a.excluded.test` is passed through without any handshake from the sidecar (`interception: excluded`), and `api.test` is still intercepted. A ClientHello offering only TLS 1.1, or ALPN `imap` only, is passed through as `unsupported` and not remembered. An inbound TLS connection is never intercepted and its line has no `interception` field. |
| TLS-22 | **I.** Close and reset on the decrypted path: a client `close_notify` + FIN after a complete request reaches the destination as `close_notify` + FIN after the request bytes, and the destination can still answer (PRX-17); a TCP reset by either side reaches the other as a reset within 1 s (PRX-18); at the end of the shutdown grace an intercepted exchange writes `result: shutdown` (PRX-20). |
| TLS-23 | **I.** After a known set of connections (intercepted, excluded, `no_ca`, `unsupported`, `upstream_untrusted`, `client_rejected`), `/metrics` shows the exact `mimic_tls_connections_total{interception}` values, `mimic_tls_passthrough_destinations`, and intercepted connections under their decrypted protocol with the SNI as `authority` in `mimic_proxy_connections_total` (§9). The PRX-08 canaries (header, cookie, query, bodies) sent over an intercepted connection appear in no log line. |
| TLS-24 | **E.** The chart creates the empty seed Secret, the Service and NetworkPolicy port 8444, and the RBAC of §11: `kubectl auth can-i` as the controller is `yes` for `create tokenreviews` and `get`/`update` of the seed Secret, `no` for other Secrets and pods; after install the Secret holds a seed; `helm uninstall` removes it. The CRD accepts `spec.tls.exclude: [api.example.com, "*.corp.example", 10.0.0.1]` and rejects `*`, `a.*.com` and 257 entries. |
| TLS-25 | **E.** In an `enabled` namespace, four client Deployments built by the e2e from `curlimages/curl`, `python:slim` (`urllib`), `node:slim` (`fetch`) and `eclipse-temurin` (JDK `HttpClient`), each with the e2e test CA installed in the image's system store, call an in-cluster HTTPS Service whose certificate comes from that CA: each call returns 200, the sidecar writes an exchange line with `tls: true` and `status: 200`, and the certificate the client saw is issued by the mimic intermediate. A call to an HTTPS Service signed by an unknown CA fails in the client as without mimic (curl exit 60) and the sidecar logs `upstream_untrusted`. A `MimicWorkload` with `spec.tls.exclude` naming the Service host makes the next pod's connections `excluded`. |
| TLS-26 | **E.** A client container that defines `SSL_CERT_FILE` in its spec to a file holding only the e2e CA keeps that value (no mimic variables for it); its first call fails, the sidecar logs one WARN `tls passthrough` with `reason: client_rejected`, and its second call returns 200 with the Service's own certificate. A sidecar token presented for another deployment's name gets `403` from the sidecar API. |

## 13. Test strategy

- **Unit** tests in `mimic-core` (pure logic) and next to the code.
- **Integration** tests per crate in `tests/`: in-process routers, or the real binary started
  as a child process.
- **End-to-end** on kind with the Helm chart (`scripts/e2e.sh`), locally and in CI.
- A criterion is covered when a test whose name contains its ID (lower case) passes.
- A timing bound ("within 1 s") is measured from the event that triggers it (the close, the
  signal), not from the start of the test, and the test's own wait for that event has its own
  larger timeout, so that a slow runner fails only when the sidecar is really late.
