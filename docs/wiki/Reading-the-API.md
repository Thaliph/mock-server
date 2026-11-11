# Reading the API

You read the discovered data as JSON, as an
OpenAPI 3.1 document per port, or in the browser ([the UI](#the-ui)).

## Reaching it

The read API is plain HTTP on `MIMIC_UI_ADDR`, default `127.0.0.1:8080` **inside each controller pod**.
It is not behind a Service. Reach it with a port-forward; Kubernetes RBAC on `pods/portforward` in the
release namespace is the authentication:

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
| `GET /v1/workloads/<ns>/<dep>/openapi` | The documents of the deployment: `inbound-<port>`, one per listening port. |
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
| `version` | `1` |
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
# redacted fields
curl -s $R | jq '.scopes.inbound["8080"].root.literals.users.param.endpoints.GET.observed.fields.fields | map_values(select(.redacted)) | keys'
# samples of one endpoint
curl -s $R | jq '.scopes.inbound["8080"].root.literals.users.param.endpoints.GET.samples | length'
```

## Load test runs

The result of a [load test](Load-test) is kept in Valkey (the last 20 runs per deployment) and served under
`loadtests`. The `<uid>` in the path is the `metadata.uid` of the `MimicLoadTest`, **not its name**: a name
answers `400`. To list the runs, then print the per-endpoint summary of one (here the run `users-1` of the
[Load test](Load-test) page):

```bash
curl -s localhost:8080/v1/workloads/demo/users/loadtests | jq -c '.loadtests[] | {uid, name, phase, reason}'
uid=$(kubectl -n demo get mimicloadtest users-1 -o jsonpath='{.metadata.uid}')
curl -s localhost:8080/v1/workloads/demo/users/loadtests/$uid |
  jq -r '.summary.endpoints[] | [.method, .template, .requests, .rps, .latencyMicros.p99] | @tsv'
# GET   /users/{users_id}   2440   40   2290
# PUT   /users/{users_id}    310    5   1980
```

(Illustrative numbers for a one-minute run: the plan is weighted by what discovery recorded, 40 `GET`s to 5
`PUT`s in the Quick start, so expect about eight `GET`s for each `PUT`; yours will differ.)

Every row (`summary.total` and each entry of `summary.endpoints`) holds `requests`, `missed`, `statuses` (by
class: `2xx`, `3xx`, `4xx`, `5xx`), `errors` (by kind), `rps` and `latencyMicros` (`p50`, `p90`, `p99`, `max`,
in microseconds). An endpoint row also has `method`, `port` and `template`. Times are UTC (RFC 3339). The
same numbers are in `kubectl get mimicloadtest users-1 -o jsonpath='{.status.results}'` while the object
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

- `/ui/` lists the deployments with data;
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
