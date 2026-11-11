# Egress aggregation

The calls an injected deployment **makes** (to
other services of the cluster or to external APIs) are recorded by each pod's sidecar and merged by the
controller, per deployment, next to the inbound endpoints.

## What a pod records

The sidecar of every pod records its outbound HTTP exchanges (cleartext, and HTTPS when it is
[intercepted](TLS-interception)) in a bounded local report, in a scope of its own per **authority**: the
host and port the app called (`users.demo.svc:8080`). The same redaction rules as for inbound apply,
with a stricter view for outbound: request headers outside a short allowlist are shown as `mimic-secret` (and `Authorization: Bearer mimic-token`),
so a credential in any header, query parameter or body never leaves the pod. A connection that cannot be
decrypted (a tunnel, a refused interception) is counted per authority and kind, without any endpoint.

`/discovery` on the sidecar's admin port holds only what the pod has **not shipped yet**.

## What the controller holds

Every `MIMIC_DISCOVERY_PUSH_INTERVAL` (30 s) a sidecar posts its report to the controller. A report with outbound
data is **version 2**; one without is version 1. The controller merges the
pushes of all the pods of a deployment, so the stored report is the **union over the pods**: one
outbound endpoint (`GET /users/{users_id}` on `users.demo.svc:8080`) counts the calls of every replica.
The sidecar clears what the controller acknowledged, as it does for inbound.

The merge is bounded: at most `MIMIC_STORE_MAX_AUTHORITIES` outbound scopes per deployment
(default 128, twice the sidecar's 64, since the store holds the union of the pods) and the per-direction
endpoint limits. When the limits bite, what is kept is chosen by traffic: authorities with the most exchanges (and
tunnel counts) first, then those already stored, so a burst of new authorities cannot push out the
providers a deployment really uses. `mimic_store_refused_total` counts what was refused. Outbound scopes expire like the rest of the
report ([Operations](Operations#retention)).

## Reading it

| Where | What |
|---|---|
| `GET /v1/workloads/<ns>/<dep>` | The pods that pushed, with the outbound endpoints in the totals. |
| `GET /v1/workloads/<ns>/<dep>/report` | The merged report; `scopes.outbound."<authority>"`. |
| `GET /v1/workloads/<ns>/<dep>/endpoints` | Every endpoint; outbound ones carry `authority` and the `document` that holds them. |
| `GET /v1/workloads/<ns>/<dep>/openapi` | One document per listening port (`inbound-<port>`) and one per authority: `outbound-<authority>`. |
| `kubectl get mimicworkload` | `Endpoints` is the total of inbound and outbound endpoints; `status.discovery.endpoints.outbound` is the outbound part. |

An outbound document has the called paths, `servers` set to `http://<authority>` (or `https://`) and the
authority in `info.x-mimic-authority`. A destination that was only tunnelled gets a document with no
paths, so you can see what cannot be documented. In the [UI](Reading-the-API#the-ui) the deployment
page's endpoint table has an **Authority** column whose cells link to the document.

## Compatibility with an older controller

During a rollout, or after a rollback, a sidecar may talk to a controller that only knows version 1
(`400`) or that refuses the push (`507`). The sidecar then posts its **inbound part only**, with the same
sequence, and keeps the outbound part locally (visible in `/discovery`, counted in
`mimic_discovery_pushes_total{result="inbound_only"}`), and one WARN `outbound not shipped` is written per
streak. The next interval tries version 2 again. If a retry cannot tell whether an earlier attempt was
applied, the outbound part is dropped rather than counted twice
(`mimic_discovery_outbound_dropped_total`).

## Pulling it back

Each sidecar pulls its deployment's aggregated egress from the controller and holds it in memory: see
[Egress endpoint](Egress-endpoint).

## Checking it

`scripts/e2e.sh` runs a `front` Deployment with two replicas and a `MimicWorkload`, calls it, and checks that
the controller's merged report holds the sum of both pods' outbound calls, that no canary secret appears
anywhere, and, in a headless browser, that the deployment page and the outbound document render.

If an outbound endpoint is missing: is the destination reached over HTTP or an intercepted HTTPS
connection? Did the pod push yet (wait two intervals)? Is `MIMIC_DISCOVERY_MAX_AUTHORITIES` or the store's
limit reached ([Troubleshooting](Troubleshooting))?
