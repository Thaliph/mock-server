# Mock mode

Covers F10. Reference: spec §7, §7.4, §8, §12.11. With `spec.mode: mock` on a `MimicWorkload`, the outbound
calls of its Deployment's pods are answered from the egress the pods already hold (see
[Egress endpoint](Egress-endpoint)). The real destination is not called: a load test can run against its
own app without hitting a provider, paying for its calls or hitting its rate limits.

## Turn it on and off

```bash
kubectl -n shop patch mimicworkload front --type merge -p '{"spec":{"mode":"mock"}}'
kubectl -n shop patch mimicworkload front --type merge -p '{"spec":{"mode":"discovery"}}'
```

- **The switch is live.** Pods pick the mode up from the sidecar's next egress pull (`MIMIC_EGRESS_PULL_INTERVAL`,
  60 s by default): within one interval a pod answers from its snapshot, and back in discovery it records again.
  No pod is restarted. Connections that are open at the switch follow the rules below.
- **Convergence.** `kubectl get mimicworkload front` prints `Mode` and `Mode pods`. `status.mode` says how many
  pods follow the effective mode (`pods`) and how many still carry the other one (`otherPods`). A load test can
  start when `otherPods` is `0` and `pods` equals the ready replicas.
- **New pods** start in the right mode: the webhook sets `MIMIC_MODE` (and the latency factor and passthrough
  list) on the sidecar of every pod it creates in mock.
- **Several MimicWorkloads** for one Deployment: the Deployment is in mock if any of them says so.

## What a mocked call gets

The sidecar answers from the **egress snapshot** of its own Deployment, and from nothing else:

1. **Match.** The request's authority, method, path and query are compared with the recorded endpoints. Header
   values are never compared, so credentials and request ids do not turn a match into a miss. The first tier
   that gives a sample wins: `exact` (same fields and body), `endpoint` (the sample sharing the most fields),
   `fallback` (the endpoint's most frequent status). The answer carries `x-mimic-match: exact|endpoint|fallback`.
2. **Answer.** The recorded status and headers, and the recorded body serialised again. Redacted values come back
   as `<redacted>`; mimic never rebuilds a secret.
3. **Latency.** A delay drawn from the recorded latency histogram of the endpoint, times the latency factor
   (`spec.mock.latencyFactor`, default 1, capped at 300 s). `0` answers at once.
4. **Miss.** No authority, endpoint or matching status: `599` with `x-mimic-miss: 1` and an empty body, at once.
   The destination is still not called. Each distinct miss is logged once per mock period (WARN `mock miss`, the
   path without its query).

gRPC calls are answered with status `200`, the recorded headers and one empty message, then `grpc-status: 0`:
recorded messages are opaque, so the content of a gRPC answer is not replayed.

An empty snapshot (nothing recorded yet) misses every call. Turn the mode on once the Deployment has traffic
behind it: [Egress aggregation](Egress-aggregation) shows what has been recorded.

## Connections

| Connection | In mock |
|---|---|
| HTTP/1.x and HTTP/2 (cleartext, or TLS intercepted) | Answered by the sidecar. Keep-alive and pipelining work; `CONNECT` and upgrades are misses. |
| TLS intercepted (on a name the sidecar intercepts) | Decrypted and answered the same way. The leaf certificate is issued for the name; no handshake with the destination. |
| TLS passed through (excluded, or not intercepted) | Closed. Nothing reaches the destination. |
| Non-HTTP TCP (databases, brokers) | Relayed to the real destination and recorded as a tunnel: mimic cannot answer them. |
| `spec.mock.passthrough` destinations | Handled as in discovery: really called and recorded. |

Nothing is connected for an HTTP or TLS connection: the sidecar waits for the first bytes to tell them apart.
A server-first protocol that sends nothing within 1 s is connected to, as in discovery.

## Switching on open connections

- **To mock.** A request read after the switch is answered by mock, never forwarded. The destination connection
  of an HTTP/1 keep-alive connection is closed after the answers already sent; an HTTP/2 connection carries on
  with mocked streams. Open TLS tunnels that mock would not keep are closed (`mock_closed`); non-HTTP TCP
  tunnels keep flowing.
- **Back to discovery.** A mocked HTTP/1 connection finishes the answers already pending, then closes; a mocked
  HTTP/2 connection gets `GOAWAY`. The next connection is forwarded and recorded again.

## Limits and rollback

- Mocked exchanges are **not recorded**: a mock never feeds its own snapshot.
- Mock answers only what was recorded. A path, method or authority that was never called in discovery is a miss.
- A sidecar keeps its mode through a container restart (state file in the pod's `mimic-trust` volume). When the
  controller is unreachable, a pod keeps its current mode; a controller outage does not turn a load test into
  real egress.
- **Roll back only out of mock.** A chart rollback that removes `spec.mode` and the status fields, while pods
  are still in mock, leaves pods that forward for real when they are recreated.
- Personal data that no detector catches is replayed to the app that made the call, as recorded (see
  [Security](Security)).

## Settings

| Where | Setting | Default | Meaning |
|---|---|---|---|
| `MimicWorkload` | `spec.mode` | `discovery` | `discovery` or `mock`. |
| `MimicWorkload` | `spec.mock.latencyFactor` | `1` | Multiplies the recorded latency (0 to 100). |
| `MimicWorkload` | `spec.mock.passthrough` | empty | Destinations that stay real in mock. |
| Sidecar | `MIMIC_MOCK_MAX_PENDING` | `1024` | Pending mocked answers; more get `599` with reason `overload`. |
| Controller | `MIMIC_STATUS_POD_WINDOW` | `5m` | How recent a pod's last pull must be to count in `Mode pods`. |

Full list: [Configuration](Configuration).
