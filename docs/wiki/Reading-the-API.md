# Reading the API

Step 5 of [Get started](Home#get-started). You read the discovered data as JSON, as an
OpenAPI 3.1 document per port, or in the browser ([the UI](#the-ui)).

## Reaching it

The read API is plain HTTP on `MIMIC_UI_ADDR`, default `127.0.0.1:8080` **inside each controller pod**.
It is not behind a Service. Reach it with a port-forward; Kubernetes RBAC on `pods/portforward` in the
release namespace is the authentication:

If a forward on 8080 from an earlier step is still running, a new one fails with "address already in use" while
`curl` quietly talks to the old one. Check `jobs` first, or stop it with `pkill -f 'port-forward.*8080'`.

```bash
kubectl -n mimic-system port-forward deploy/mimic-controller 8080:8080 >/dev/null &
PF=$!
# wait until the forward answers (a few seconds), then use it; `kill $PF` stops it
for i in $(seq 30); do curl -fs localhost:8080/v1/workloads >/dev/null && break; sleep 1; done
```

The controller has two HTTP ports: **8081** is the admin port (`/healthz`, `/readyz`, `/metrics`) and is on the
`mimic-controller` Service; **8080** is this read API and the UI, on the loopback of the pod only, so forward
`deploy/mimic-controller`, not `svc/mimic-controller`.

All replicas read the same Valkey, so any pod gives the same answer. Safeguards: a peer that is not
loopback gets `403`; so does a request whose `Host` is missing or not `localhost`, `127.0.0.1` or
`[::1]` (DNS rebinding). `GET` and `HEAD` (the same status and headers, no body); every other method gets
`405` with `Allow: GET, HEAD`. JSON with sorted keys. Nothing can be written or
deleted: data leaves by expiry ([Operations](Operations#retention)).

## Endpoints

| Endpoint | Answer |
|---|---|
| `GET /v1/workloads` | Summary of every deployment with data, sorted by namespace then deployment. |
| `GET /v1/workloads/<ns>/<dep>` | The same summary plus `pods`. |
| `GET /v1/workloads/<ns>/<dep>/report` | The stored merged report. |
| `GET /v1/workloads/<ns>/<dep>/endpoints` | Every endpoint: direction, port, method, template, exchanges, statuses, latency quantiles, hosts, last seen and the `document` that holds it. |
| `GET /v1/workloads/<ns>/<dep>/loadtests` | The stored [load test](Load-test) runs of the deployment, newest first: `uid`, `name`, `phase`, `reason`, `start`, `end` and `total`. |
| `GET /v1/workloads/<ns>/<dep>/loadtests/<uid>` | One run, with its `summary`: the totals and one row per endpoint (`400` for a uid that is not a UUID, `404` for an unknown one). |
| `GET /v1/resources` | Every deployment named by a `MimicWorkload` (or a `MimicLoadTest`), with its `mode` and the `kind` and `name` of those resources. Read from the controller's watch, not from the store, so a deployment with no data yet is listed. |
| `GET /v1/workloads/<ns>/<dep>/resources` | The mimic custom resources that configure the deployment, sorted by kind then name: `metadata` (creation time, generation, labels, annotations), `spec` and its read-only `yaml`, the status `conditions` and, for a `MimicLoadTest`, its `phase`. `managedFields` and the `last-applied` annotation are left out, and a load test's `credentials` shows the Secret's name and the fields it fills, never a key or a value. `400` for an invalid name; an empty list when nothing configures it. |
| `GET /v1/workloads/<ns>/<dep>/openapi` | The documents of the deployment: `inbound-<port>`, one per listening port, and `outbound-<authority>`, one per destination it called (`front` has `outbound-users.demo.svc:8080`). |
| `GET /v1/workloads/<ns>/<dep>/openapi/<id>` | The OpenAPI 3.1 document `<id>` (`400` for an invalid id, `404` for one the report does not hold). |

Status codes: `404` deployment without data, `400` a name that is not DNS-1123, `503` Valkey
unavailable, `500` a stored value that does not parse (the controller logs an ERROR, see below).

### List

```bash
curl -s localhost:8080/v1/workloads
```

```json
{"workloads":[{"deployment":"users","endpoints":2,"exchanges":45,"last_push":"2026-11-02T10:00:21.000Z","namespace":"demo","sample_bytes":41230}]}
```

`endpoints` is the sum over directions; `last_push` is the last applied push (UTC, RFC 3339).

### One deployment

```bash
curl -s localhost:8080/v1/workloads/demo/users
```

```json
{"deployment":"users","endpoints":2,"exchanges":45,"last_push":"2026-11-02T10:00:21.000Z","namespace":"demo","pods":{"users-5d9c7b8f6-8xk2p":"2026-11-02T10:00:21.000Z","users-5d9c7b8f6-q7m4t":"2026-11-02T10:00:19.000Z"},"sample_bytes":41230}
```

`pods` maps pod name to its last push (applied or retried). A deleted pod's entry expires after
`min(24 h, MIMIC_STORE_TTL)`.

### The report

The report is the same JSON document a sidecar serves at `/discovery`, merged over all pods, with
`pod: null`, and with a `last_seen` day (days since 1970-01-01) on endpoints and samples. It is a
versioned tree, not an OpenAPI file:

| Key | Content |
|---|---|
| `version` | `1`; `2` when the report holds outbound scopes; `3` when a sample holds a gRPC message or trailers (see "gRPC calls") |
| `pod` | `null` in the merged report |
| `exchanges` | Exchanges covered |
| `scopes.<direction>.<port>.root` | Path template tree: `literals` (name to child node), `param` (child node for parameters), `absorbing`, and `endpoints` (method to stats) |
| `scopes.<direction>.<port>.raw` | gRPC paths (never templated): path, then method |

An endpoint's stats hold `statuses` (status code to count), `latency` (sparse log buckets),
`hosts`, `observed` (query, request and response headers and bodies with schemas, and `fields`: volatile
tracking, with `redacted` marks), `samples` and `last_seen`. Trimmed excerpt of the report of a
`GET /users/{id}` endpoint (path `/users/<n>` on port 8080):

```json
{
  "version": 1, "pod": null, "exchanges": 4,
  "scopes": {"inbound": {"8080": {"root": {"literals": {"users": {
    "param": {"absorbing": false, "endpoints": {"GET": {
      "statuses": {"200": 4},
      "hosts": ["users.demo.svc:8080"],
      "observed": {
        "request":  {"headers": {"exchanges": 4, "keys": {
          "authorization": {"count": 4, "array": false, "schema": {"kinds": ["String"]}}}}},
        "response": {"bodies": {"application/json": {"count": 4, "bytes": 208,
          "json": {"kinds": ["Object"], "object": {
            "props": {"active": {"kinds": ["Boolean"]}, "id": {"kinds": ["String"]},
                      "name": {"kinds": ["String"]}, "score": {"kinds": ["Number"]}},
            "required": ["active", "id", "name", "score"]}}}}},
        "fields": {"fields": {"request.header/authorization": {"n": 4, "redacted": true}}}},
      "samples": {"814356248389461051": {
        "request":  {"method": "GET", "path": "/users/4", "query": [],
                     "headers": [["authorization", "Bearer mimic-token"], ["accept", "application/json"]],
                     "body": null},
        "response": {"status": 200, "headers": [["content-type", "application/json"]],
                     "body": {"media": "application/json",
                              "value": {"active": true, "id": "1", "name": "user 1", "score": 1.5}}},
        "head_ms": 7}}}}}}}}}}
}
```

(Fields such as `latency`, `sketch`, `format`, `items`, `request_shape` and `bytes` are omitted here.)
The path template (`/users/{users_id}`) is derived when rendering: the tree stores `users` as a literal
and its `param` child. The format is internal and versioned; the OpenAPI 3.1 view is the next section.

Useful `jq` one-liners:

```bash
R=localhost:8080/v1/workloads/demo/users/report
# methods and status counts under /users/{param}
curl -s $R | jq '.scopes.inbound["8080"].root.literals.users.param.endpoints | map_values(.statuses)'
# redacted fields (with the Quick start traffic: the GET shows only the response cookie, the PUT also the
# Authorization header and the password, the only requests that sent them)
curl -s $R | jq '.scopes.inbound["8080"].root.literals.users.param.endpoints | map_values(.observed.fields.fields | map_values(select(.redacted)) | keys)'
# samples of one endpoint
curl -s $R | jq '.scopes.inbound["8080"].root.literals.users.param.endpoints.GET.samples | length'
```

### gRPC calls

A call whose `content-type` is `application/grpc` (or `application/grpc+proto`) is recorded like any
other, as an endpoint of `scopes.<direction>.<port>.raw` keyed by its path (`/pkg.Users/Get`), and from
version 3 its samples hold the **first message** of the request and of the response, and the response
**trailers**. A message is read without a schema, so its fields are keyed by number, and it is
redacted before anything is stored: JWTs, tokens, keys and cards become their placeholders
(`mimic-jwt`, `mimic-token`, ...), a string or bytes value that looks like a random key becomes
`mimic-secret`, and the values of `-bin` metadata are never kept. No raw frame is stored. Trimmed
excerpt of a sample:

```json
{"request":  {"method": "POST", "path": "/pkg.Users/Get", "query": [],
              "headers": [["authorization", "Bearer mimic-token"], ["content-type", "application/grpc"]],
              "body": {"media": "application/grpc", "value": {"1": "ann", "2": "mimic-jwt"}}},
 "response": {"status": 200, "headers": [["content-type", "application/grpc"]],
              "body": {"media": "application/grpc", "value": {"1": "ann", "3": {"1": 7}}},
              "trailers": [["grpc-status", "0"], ["x-trace-bin", "mimic-secret"]]}}
```

Fields that hold more than one value are arrays; a field that is not a string or a number is an object
with one key that starts with `$` (`{"$bytes": "<base64>"}`, `{"$fixed32": n}`, `{"$fixed64": n}`). A
message that is compressed with an encoding other than `gzip`, over 256 KiB, or not a protobuf message
is left out (`mimic_discovery_bodies_total{result}` says which: `opaque`, `too_large`, `encoding`).
A mock replays the response message of a unary call and the sequence of a server stream
(see [Mock mode](Mock-mode)). The OpenAPI document of a gRPC operation shows the message as an
`application/grpc` request and response with the field numbers as property names.

**Server streaming.** When the request holds exactly one message, the response messages after the first
(at most 63, so 64 in all, and 256 KiB of JSON together) are the `stream` of the response, a list of
`{"delay_ms": 108, "message": {...}}`: each message is redacted exactly like the first, and `delay_ms` is the
time since the message before it, rounded up to the bound of its latency bucket (`0`, `1`, `2`, ... `54`, `108`,
`216`...: never an exact time). A client-streaming or bidirectional call is recorded by its first messages as
above and has no `stream`. Only the report (and its `format` 2 egress set) holds the steps; the OpenAPI
document shows the first message.

```json
"response": {"status": 200, "body": {"media": "application/grpc", "value": {"1": 1}},
             "stream": [{"delay_ms": 108, "message": {"1": 2}}, {"delay_ms": 108, "message": {"1": 3}}],
             "trailers": [["grpc-status", "0"]]}
```

A report that holds no gRPC message is the same as before, and the controller keeps serving sidecars of
earlier versions: the egress set (`/egress`, a sidecar's view of the calls its app makes) is served as
`format` 1, which leaves the gRPC parts out, to a sidecar that asks for it and as `format` 2 to one that
asks for the gRPC parts.

## Load test runs

The result of a [load test](Load-test) is kept in Valkey (the last 20 runs per deployment) and served under
`loadtests`. The `<uid>` in the path is the `metadata.uid` of the `MimicLoadTest`, **not its name**: a name
answers `400`. To list the runs, then print the per-endpoint summary of one (here the run `front-1` of the
[Load test](Load-test) page, on the deployment `front`):

```bash
curl -s localhost:8080/v1/workloads/demo/front/loadtests | jq -c '.loadtests[] | {uid, name, phase, reason}'
uid=$(kubectl -n demo get mimicloadtest front-1 -o jsonpath='{.metadata.uid}')
curl -s localhost:8080/v1/workloads/demo/front/loadtests/$uid |
  jq -r '.summary.endpoints[] | [.method, .template, .requests, .rps, .latencyMicros.p99] | @tsv'
# GET   /front/users/{users_id}   2440   40   2290
# PUT   /front/users/{users_id}    310    5   1980
```

(Illustrative numbers for a one-minute run: the plan is weighted by what discovery recorded, 40 `GET`s to 5
`PUT`s in the Quick start, so expect about eight `GET`s for each `PUT`; yours will differ.)

Every row (`summary.total` and each entry of `summary.endpoints`) holds `requests`, `missed`, `statuses` (by
class: `2xx`, `3xx`, `4xx`, `5xx`), `errors` (by kind), `rps` and `latencyMicros` (`p50`, `p90`, `p99`, `max`,
in microseconds). An endpoint row also has `method`, `port` and `template`. Times are UTC (RFC 3339). The
same numbers are in `kubectl get mimicloadtest front-1 -o jsonpath='{.status.results}'` while the object
exists; the read API keeps them after it is deleted.

## OpenAPI documents

```bash
curl -s localhost:8080/v1/workloads/demo/users/openapi | jq -c .documents
curl -s localhost:8080/v1/workloads/demo/users/openapi/inbound-8080 > users.openapi.json
```

The document is inferred from observed traffic and built from the stored, redacted report only: paths
are the discovered templates (`/users/{users_id}`), with parameters, bodies and responses taken from
the recorded schemas and up to 20 samples as `examples`. `servers` lists the hosts the sidecar saw
(documentation of how to reach the service; nothing calls it), `security` follows the `Authorization`
schemes seen (the credential itself is never stored: samples show `Bearer mimic-token`), and `x-mimic-*` fields carry counts, latency
quantiles, hosts and last-seen days. Import the JSON into any OpenAPI 3.1 tool.

## The UI

The same listener serves a small read-only UI. With the port-forward above, open
`http://localhost:8080/ui/`:

- the header has four tabs: Map, Workloads, Endpoints and Load tests. Map and Load tests are placeholders for now;
- `/ui/` is the Workloads tab: every deployment with its mode, pods, endpoints, last push and a link to each mimic resource that configures it (`MimicWorkload`, `MimicLoadTest`);
- `/ui/workloads/<ns>/<dep>/resources/<kind>/<name>` is the resource view: the spec as read-only YAML, the labels and annotations, and the status conditions. Annotation values are shown as their authors set them, as plain text (the UI is loopback-only and writes every value as text, never as markup);
- `/ui/endpoints` is the Endpoints tab: pick a deployment (the pick is kept in the address, `/ui/endpoints?deployment=<ns>/<dep>`, so you can bookmark or share a view) to list its inbound and outbound endpoints, each with a link to the Swagger UI page of its document. Four filters narrow the table in the browser, without a new request: method, template and authority (case-insensitive substring), and status class (`2xx` to `5xx`: the endpoint has at least one recorded status of that class);
- `/ui/loadtests` is the Load tests tab: the stored runs of every deployment (deployment, name, phase, reason, requests, RPS, p99, start and end), newest first for each deployment. The name links to the run page, `/ui/workloads/<ns>/<dep>/loadtests/<uid>`, which shows the total, the error kinds with their counts and one row per endpoint (requests, missed, RPS, statuses, errors, p50, p90, p99 and max in microseconds). When load tests are not enabled in the chart (`loadTest.enabled`), the tab says so and shows nothing else; it reads `GET /v1/features` (`{"loadTest": {"enabled": false}}`) to know. No credential field, value or Secret key is ever shown, and a run cannot be started or stopped from the UI;
- `/ui/workloads/<ns>/<dep>` shows the summary, the pods, the endpoints and one link per document;
- the deployment page also has a "Load tests" table, filled from the routes above;
- `/ui/workloads/<ns>/<dep>/openapi/<id>` shows the document in Swagger UI, with a link to the raw JSON.

Swagger UI is embedded in the controller (no CDN, nothing fetched from the internet) and has no "Try
it out" button: the UI never sends a request to your workloads, it only reads the store. The pages run
under a strict Content Security Policy (no inline script, same origin only) and show recorded text
as text, so a hostile request body cannot run script in your browser. Use `localhost` in the URL: the
`Host` check refuses other names.

## Local report of one pod

Each sidecar serves its **not yet shipped** report at `GET /discovery` on the admin port 15100, to
loopback peers only (any other peer gets `403`), so use a port-forward:

```bash
kubectl -n demo port-forward pod/<pod> 15100:15100
curl -s localhost:15100/discovery | jq .exchanges
```

## Controller logs for read errors

| ERROR message | Fields | Meaning |
|---|---|---|
| `stored report unreadable` | `namespace`, `deployment` | `:report` does not parse; read answers `500`, pushes `503`; the value is never overwritten. |
| `stored meta unreadable` | same | `:meta` does not parse; `500`. |
| `stored pods unreadable` | same | `:pods` does not parse; `500`. |

See [Troubleshooting](Troubleshooting) for the fix.

## `MimicWorkload` status

The summary is also in `kubectl get mimicworkload` and `status.discovery` ([Configuration](Configuration)).

## Clean up

The last step of Get started. Stop a port-forward you still have running (Ctrl-C, or `kill $PF` in the shell that
started it), then delete the demo. Deleting the namespace is enough, whatever mode the apps are in. The read API keeps the `demo/*` entries until
they expire (`MIMIC_STORE_TTL`, 30 days after the last push by default, [Operations](Operations#retention));
there is nothing to do about it:

```bash
kubectl delete namespace demo
cd .. && rm -r mimic-demo
```

On kind, `kind delete cluster --name mimic` removes everything, mimic included. On a cluster you keep, see
[Uninstall](Install#uninstall).
