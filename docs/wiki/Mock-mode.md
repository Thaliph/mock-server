# Mock mode

With `spec.mode: mock` on a `MimicWorkload`, the outbound
calls of its Deployment's pods are answered from the egress the pods already hold (see
[Egress endpoint](Egress-endpoint)). The real destination is not called: a load test can run against its
own app without hitting a provider, paying for its calls or hitting its rate limits.

Step 3 of [Get started](Home#get-started). The examples use the [Quick start](Quick-start) apps in the namespace
`demo`: `front`, which calls `users`, so `front` is the one that is mocked. Its `MimicWorkload` needs the
recorded calls to `users` (the outbound rows of its `endpoints` list).

## Try it: `front` without `users`

First a call in discovery, to compare. The client reaches `front`; `front` calls `users`. The caller of
`front` never sees the headers of the answer of `users` (`front` returns only its status and body), so the
second command makes the call from `front`'s own container, to see what its app gets:

```bash
kubectl -n demo exec client -- python3 -c '
import urllib.request
print(urllib.request.urlopen("http://front.demo.svc:8080/front/users/7", timeout=10).read().decode())'
kubectl -n demo exec deploy/front -c app -- python3 -c '
import urllib.request
r = urllib.request.urlopen("http://users.demo.svc:8080/users/7", timeout=10)
print(r.status, r.headers.get("x-mimic-match"), r.read().decode())'
# {"id": "7", "name": "user 7", "active": true, "score": 1.5}
# 200 None {"id": "7", ...}
```

Switch `front` to mock and wait until its pod follows (up to one pull interval; the wait loop is
[below](#turn-it-on-and-off)):

```bash
kubectl -n demo patch mimicworkload front --type merge -p '{"spec":{"mode":"mock"}}'
for i in $(seq 60); do
  kubectl -n demo get mimicworkload front -o json |
    jq -e '.status.mode | .mode == "mock" and .otherPods == 0 and .pods == 1' >/dev/null && break
  sleep 3
done
```

Now switch `users` off, and call `front` again. It still answers: its sidecar answers the call to `users` from
the recording.

```bash
kubectl -n demo scale deploy/users --replicas=0
kubectl -n demo exec client -- python3 -c '
import urllib.request
print(urllib.request.urlopen("http://front.demo.svc:8080/front/users/7", timeout=10).read().decode())'
kubectl -n demo exec deploy/front -c app -- python3 -c '
import urllib.request
r = urllib.request.urlopen("http://users.demo.svc:8080/users/99", timeout=10)
print(r.status, r.headers.get("x-mimic-match"), r.read().decode())'
# 200 fallback {"id": "...", "name": "user ...", ...}
```

The answer is a recorded one: its body is a sample of `GET /users/{users_id}`, so the id may not be the one
you asked for, and it is serialised again (compact, sorted keys), not byte for byte. `x-mimic-match` says it
was mocked, and which tier matched: `exact`, `endpoint` or `fallback`, depending on how close your request is
to a recorded one. With the Quick start traffic and an id it never sent, you get `fallback`. It is visible to the
app that made the call, here `front`'s app. If the answer is `599` with `x-mimic-miss: 1` instead, the sidecar
has not pulled the recording yet: wait a minute and retry. The same fact is in the access log of `front`'s
sidecar, the line has a `mock` field:

```bash
kubectl -n demo logs deploy/front -c mimic-sidecar | grep '"mock"' | tail -n 2
```

Switch `users` back on:

```bash
kubectl -n demo scale deploy/users --replicas=2
kubectl -n demo rollout status deploy/users --timeout=3m
```

`front` stays in mock for the [Load test](Load-test), which needs it. To go back to live calls (the `users` pods
must be running first): `kubectl -n demo patch mimicworkload front --type merge -p '{"spec":{"mode":"discovery"}}'`.

## Turn it on and off

```bash
# the Quick start apps (demo/front calls demo/users); replace the namespace and the name with your own MimicWorkload
kubectl -n demo patch mimicworkload front --type merge -p '{"spec":{"mode":"mock"}}'
kubectl -n demo patch mimicworkload front --type merge -p '{"spec":{"mode":"discovery"}}'
```

- **The switch is live.** Pods pick the mode up from the sidecar's next egress pull (`MIMIC_EGRESS_PULL_INTERVAL`,
  60 s by default): within one interval a pod answers from its snapshot, and back in discovery it records again.
  No pod is restarted. Connections that are open at the switch follow the rules below.
- **Convergence.** `kubectl -n demo get mimicworkload front` prints `Mode` and `Mode pods`. `status.mode` says how many
  pods follow the effective mode (`pods`) and how many still carry the other one (`otherPods`). A load test can
  start when `otherPods` is `0` and `pods` equals the ready replicas. To wait for it from a shell (the
  [Quick start](Quick-start) app, `front` with 1 replica; it takes up to one pull interval, and gives up after
  3 minutes):

  ```bash
  kubectl -n demo patch mimicworkload front --type merge -p '{"spec":{"mode":"mock"}}'
  for i in $(seq 60); do
    kubectl -n demo get mimicworkload front -o json |
      jq -e '.status.mode | .mode == "mock" and .otherPods == 0 and .pods == 1' >/dev/null && break
    sleep 3
  done
  kubectl -n demo get mimicworkload front -o jsonpath='{.status.mode}{"\n"}'   # {"mode":"mock","otherPods":0,"pods":1}
  ```

  An app that makes no outbound calls, like `users`, has nothing to mock: the switch is valid, but only an
  app that calls others, like `front`, shows the effect.
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
   as their placeholder (`{"access_token": "mimic-token"}`, `set-cookie: sid=mimic-cookie`); mimic never rebuilds
   a secret and no Secret replaces a placeholder in a mock answer. A request field that holds a placeholder is
   not compared, so an app that sends `Authorization: Bearer mimic-token` still matches. A test that needs real
   values uses the [load test](Load-test) credentials instead.
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
| `MimicWorkload` | `spec.mode` | `none` | `none`, `discovery` or `mock`. Leave `mock` for `discovery` or `none` and mocked connections close once their answers are sent. A pod in `none` records nothing, so a deployment must be in `discovery` first to have recordings to mock. |
| `MimicWorkload` | `spec.mock.latencyFactor` | `1` | Multiplies the recorded latency (0 to 100). |
| `MimicWorkload` | `spec.mock.passthrough` | empty | Destinations that stay real in mock. |
| Sidecar | `MIMIC_MOCK_MAX_PENDING` | `1024` | Pending mocked answers; more get `599` with reason `overload`. |
| Controller | `MIMIC_STATUS_POD_WINDOW` | `5m` | How recent a pod's last pull must be to count in `Mode pods`. |

Full list: [Configuration](Configuration).
