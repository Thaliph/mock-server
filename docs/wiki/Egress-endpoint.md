# Egress endpoint

Covers F9. Reference: spec §7.7. The controller serves every sidecar the **egress** of its own deployment (the
[outbound endpoints it aggregated](Egress-aggregation)), and the sidecar keeps it in memory. F9 changes no
traffic: the snapshot is held, measured and summarised, never used to answer a request. From F10 it is what
the sidecar mocks outbound calls from.

## What it serves

`GET /v1/workloads/<deployment>/egress?format=1` on the sidecar API (the HTTPS listener of the pushes, same
authentication: the pod's projected token, the namespace from the token). A pod reads its own deployment only.

The answer is a **projection** of the stored outbound part, and nothing more than a mock needs:

```json
{"format": 1, "authorities": {"users.disc-22.svc:8080": {"endpoints": [
  {"method": "GET", "template": "/users/{users_id}", "grpc": false, "statuses": {"200": 12},
   "latency": {...}, "volatile": ["..."], "redacted": ["..."], "samples": [...]}]}}}
```

One entry per authority that has at least one endpoint. There is no schema, origin, host list or pod name. The
samples keep the redaction of the report they come from. A deployment with no outbound data answers `200` with
`{"authorities": {}, "format": 1}`.

The answer carries an `ETag` (`"<format>-<digest>"`) and `Cache-Control: no-store`. The sidecar sends it back in
`If-None-Match`; an unchanged egress gets `304` without reading the stored report. The controller caches the
projection per namespace, deployment, format and digest, and builds it once for concurrent pulls.

## What the sidecar does

With `MIMIC_CONTROLLER_URL` set, the sidecar pulls every `MIMIC_EGRESS_PULL_INTERVAL` (first pull after a random
delay within one interval), in every mode. A `200` replaces the snapshot, a `304` keeps it. A body over
`MIMIC_EGRESS_MAX_BYTES` is not read further and the snapshot is kept. Failures are retried with the push
backoff.

The local view is on the admin port, loopback only:

```
kubectl -n <ns> port-forward <pod> 15100:15100
curl localhost:15100/egress
```

It lists the authorities with their endpoints, methods, statuses and exchange counts, the `etag` and the time of
the last pull. It never shows a sample or a field name.

## Knobs

| Variable | Default | Where | Meaning |
|---|---|---|---|
| `MIMIC_EGRESS_PULL_INTERVAL` | `60s` | sidecar | Period of the pull, 1s-1h. Not set by the webhook. |
| `MIMIC_EGRESS_MAX_BYTES` | `4Mi` | sidecar | Largest egress set read, 64Ki-1Gi. Sized so that the pull peak and the discovery worst case fit the 128 Mi sidecar limit: raising it needs a larger `MIMIC_INJECT_SIDECAR_RESOURCES` memory limit. |
| `MIMIC_SIDECAR_API_MAX_EGRESS` | `4Mi` | controller | Largest egress set answered, 64Ki-1Gi; a larger one gets `507`. Keep it at most the sidecars' `MIMIC_EGRESS_MAX_BYTES`. |
| `MIMIC_SIDECAR_API_EGRESS_BUDGET` | `32Mi` | controller | Bytes of egress sets cached or being sent at once per replica; at least `MAX_EGRESS`, at most 4Gi. When a new projection does not fit, the least recently served ones are dropped, then the pull gets `503`. |

A build also takes a share of `MIMIC_SIDECAR_API_BODY_BUDGET` (the stored size plus `MIMIC_SIDECAR_API_MAX_EGRESS`).

## Metrics

Sidecar:

- `mimic_egress_pulls_total{result}`: `ok`, `not_modified`, `too_large`, `invalid`, `unsupported`, `error`.

Controller:

- `mimic_sidecar_api_requests_total{endpoint="/v1/workloads/{deployment}/egress",result}`: every pull, with its
  outcome (`ok`, `not_modified`, a refusal).
- `mimic_sidecar_api_egress_builds_total{result}`: projections built (`ok`, `too_large`, `unreadable`, `error`).
  `ok` is counted when the projection is built, **before** the admission to the egress budget: a build that is
  then refused with `503` still counts as `ok`. Read the `503` in `requests_total`.
- `mimic_sidecar_api_egress_build_duration_seconds`: time of one build.
- `mimic_sidecar_api_egress_cache_bytes`: bytes of `MIMIC_SIDECAR_API_EGRESS_BUDGET` held by projections, cached
  or being sent. It counts the held projections, not the builds in flight.

Each pull is also one `sidecar_api` log line on the controller (`endpoint` `/v1/workloads/{deployment}/egress`,
`result`). Sidecar logs use the target `egress`: WARN `egress pull failed` once per failure streak, INFO
`egress pull recovered`, WARN `egress rejected` for a body the check refuses. No log holds the token or the
snapshot.

## Troubleshooting

**`/egress` shows no authority.** The controller holds no outbound data for the deployment yet: the pods must
have pushed (wait two push intervals), then the sidecar pulls (up to one `MIMIC_EGRESS_PULL_INTERVAL`). Check the
`etag` and `pulled` fields, and `mimic_egress_pulls_total`.

**`pulled` is empty and the pulls count `unsupported`.** The controller is older than F9 (`404`/`405`). Upgrade
it first.

**Pulls count `too_large`.** The set is over `MIMIC_EGRESS_MAX_BYTES` (sidecar), or the controller answered `507`
(`egress limit reached`, `reason` `projection` or `stored`): raise `MIMIC_SIDECAR_API_MAX_EGRESS` and the
sidecars' `MIMIC_EGRESS_MAX_BYTES` together, and the injected memory limit.

**Pulls count `invalid`.** WARN `egress rejected` names the `reason`; the previous snapshot is kept.

**Pulls count `error`.** `503` with `Retry-After`: Valkey is unavailable or the egress budget is spent
(`mimic_sidecar_api_egress_cache_bytes` near `MIMIC_SIDECAR_API_EGRESS_BUDGET`). `401`/`403`: the token is
refused. `429`: the source is rate limited (`MIMIC_SIDECAR_API_RATE`).

**Controller ERROR `stored report unreadable` (`key` `outbound`) and pulls get `500`.** The stored outbound part
fails the structural check; it is never overwritten. See [Troubleshooting](Troubleshooting#no-data).

## Checking it

`scripts/e2e.sh` checks that both pods of an injected `front` hold `users.disc-22.svc:8080` with its endpoints and
the same `etag`, that a second pull gets `304`, that a pod of another deployment sees no authority, that no
canary secret appears in `/egress` or in any log, and that the called service's access lines do not change.
