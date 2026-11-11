# Quick start

Audience: developers. You will inject two small Python services, send traffic, and read the API that
mimic discovered. About 10 minutes.

You need a cluster with mimic installed ([Install](Install); on a laptop, [Try it on kind](Install#try-it-on-kind)),
`kubectl`, `curl` and `jq`. No clone of the repository is needed: the two demo services are three small files
(`service.py`, `users.yaml`, `front.yaml`) that you download at the tag of the release you installed.

```
client pod --> front (service A, namespace demo-client, not injected)
                 |  GET /front/users/<id>  -->  forwards to users
                 v
               users (service B, namespace demo, 2 replicas, injected)  <-- mimic records these
```

`front` forwards `/front/<path>` to `users`, passing `Authorization`, `X-Tenant-Key` and
`Content-Type`. `users` answers `GET` and `PUT /users/<id>` with JSON. Both run stdlib Python from a
ConfigMap, so no image is built.

## 1. Namespaces

`users` lives in a namespace that mimic injects; `front` and the client live in a plain one.

```bash
kubectl create namespace demo
kubectl label namespace demo mimic.platform.io/inject=enabled pod-security.kubernetes.io/enforce=privileged
kubectl create namespace demo-client
```

## 2. Deploy the services

The files are the test services of the project; their comments mention names used by its own tests, which you
can ignore. Download them at the tag of the release (here `0.2.0-alpha.2`) into a scratch directory:

```bash
APPS=https://raw.githubusercontent.com/Thaliph/mock-server/v0.2.0-alpha.2/scripts/e2e/apps
mkdir -p mimic-demo && cd mimic-demo
curl -fsSLO $APPS/service.py && curl -fsSLO $APPS/users.yaml && curl -fsSLO $APPS/front.yaml
```

`front.yaml` calls `users` by its full name, `users.disc-22.svc`: `disc-22` is the namespace of the project's own
tests. The `sed` below replaces it with the namespace you created above (`demo`); if you picked another name for
that namespace, put it there instead.

```bash
kubectl -n demo create configmap apps-code --from-file=service.py
kubectl -n demo apply -f users.yaml

kubectl -n demo-client create configmap apps-code --from-file=service.py
sed 's/users\.disc-22\.svc/users.demo.svc/' front.yaml | kubectl -n demo-client apply -f -
```

Wait until everything is ready (the first start pulls the Python image):

```bash
kubectl -n demo rollout status deploy/users --timeout=3m
kubectl -n demo-client rollout status deploy/front --timeout=3m
kubectl -n demo-client wait --for=condition=Ready pod/client --timeout=180s
```

Check the injection:

```bash
kubectl -n demo get pod -l app=users -o jsonpath='{.items[0].spec.initContainers[*].name}{"\n"}'
# mimic-init mimic-sidecar mimic-trust-0
```

## 3. Turn discovery on for `users`

Discovery is the default mode. Creating a `MimicWorkload` is what gives you a status to look at (and
is how you set per-deployment options, see [Configuration](Configuration)):

```bash
kubectl apply -f - <<'EOF'
apiVersion: mimic.platform.io/v1alpha1
kind: MimicWorkload
metadata: {name: users, namespace: demo}
spec:
  deployment: users
EOF
```

In an `enabled` namespace the `MimicWorkload` is not needed for injection; had you labelled the
namespace `selected`, only Deployments named by a `MimicWorkload` would be injected, and the pods
would need a restart (`kubectl -n demo rollout restart deploy/users`).

## 4. Send traffic

40 `GET`s, then 5 `PUT`s with a body that contains a password and a bearer token:

```bash
kubectl -n demo-client exec -i client -- python3 - <<'PY'
import json, urllib.request
base = "http://front.demo-client.svc:8080/front/users"
for n in range(1, 41):
    urllib.request.urlopen(f"{base}/{n}", timeout=10).read()
for n in range(1, 6):
    body = json.dumps({"name": f"user {n}", "password": "s3cret-value"}).encode()
    request = urllib.request.Request(
        f"{base}/{n}", data=body, method="PUT",
        headers={"Content-Type": "application/json", "Authorization": "Bearer abc.def"},
    )
    urllib.request.urlopen(request, timeout=10).read()
print("done")
PY
```

## 5. Wait for the push

Each sidecar posts its report every 30 s (`MIMIC_DISCOVERY_PUSH_INTERVAL`; the first push comes after
a random delay within that interval). The `MimicWorkload` status refreshes every 30 s more
(`MIMIC_STATUS_INTERVAL`). Wait about a minute, or let a loop wait until the status shows the 45 requests
(it gives up after 3 minutes):

```bash
for i in $(seq 36); do
  [ "$(kubectl -n demo get mimicworkload users -o jsonpath='{.status.discovery.exchanges}')" -ge 45 ] 2>/dev/null && break
  sleep 5
done
```

Optional: look at what one pod holds *before* it is shipped. `/discovery` is served on the sidecar
admin port to loopback peers only, so use a port-forward:

```bash
pod=$(kubectl -n demo get pod -l app=users -o jsonpath='{.items[0].metadata.name}')
kubectl -n demo port-forward "pod/$pod" 15100:15100 >/dev/null &
PF1=$!
for i in $(seq 30); do curl -fs localhost:15100/discovery >/dev/null && break; sleep 1; done
curl -s localhost:15100/discovery | jq '.exchanges'
kill $PF1
```

After a push is acknowledged, the shipped counters and samples leave the local report, so this number
drops back to what was recorded since.

## 6. Read the merged report

The read API listens on port 8080, on the loopback of each controller pod only (port 8081, the admin port, is
the one on the Service). Reach it with a port-forward (Kubernetes RBAC on `pods/portforward` in the release
namespace is the authentication); the loop waits until the forward answers:

```bash
kubectl -n mimic-system port-forward deploy/mimic-controller 8080:8080 >/dev/null &
PF=$!
for i in $(seq 30); do curl -fs localhost:8080/v1/workloads >/dev/null && break; sleep 1; done

curl -s localhost:8080/v1/workloads | jq
curl -s localhost:8080/v1/workloads/demo/users | jq
```

```json
{
  "workloads": [
    {"deployment": "users", "endpoints": 2, "exchanges": 45, "last_push": "2026-11-02T10:00:21.000Z",
     "namespace": "demo", "sample_bytes": 41230}
  ]
}
```

(Numbers and times in the outputs below are illustrative.) The merged report is the stored report document. List the endpoints it holds (inbound, by the
application port `8080`), with their status counts:

```bash
curl -s localhost:8080/v1/workloads/demo/users/report > report.json
jq '.scopes.inbound["8080"].root.literals.users.param.endpoints | map_values(.statuses)' report.json
# { "GET": {"200": 40}, "PUT": {"200": 5} }
```

That is `GET /users/{users_id}` and `PUT /users/{users_id}`: the ids were recognised as a parameter and
named after the preceding segment. The report also holds the response schemas, volatile fields,
latency histograms and up to 20 redacted samples per endpoint. More in
[Reading the API](Reading-the-API).

Check that the secrets are not there:

```bash
grep -c 's3cret-value' report.json    # 0
grep -o 'Bearer mimic-token' report.json | sort -u
# Bearer mimic-token
```

Or look at it in the browser, with the same port-forward: open <http://localhost:8080/ui/>, pick
`demo/users`, then its document. Swagger UI shows `GET` and `PUT /users/{users_id}` with the recorded
examples and the `http_bearer` security scheme (never the token). The raw OpenAPI 3.1 file is
`curl -s localhost:8080/v1/workloads/demo/users/openapi/inbound-8080`.

## 7. See it in kubectl

```bash
kubectl -n demo get mimicworkload
```

```
NAME    MODE        ENDPOINTS   PODS   LAST PUSH   MODE PODS
users   discovery   2           2      25s         2
```

`MODE PODS` is the number of pods that follow the mode ([Mock mode](Mock-mode)). `ENDPOINTS` is the inbound endpoint count, `PODS` the pods that pushed within the last 5 minutes
(`MIMIC_STATUS_POD_WINDOW`), `LAST PUSH` the age of the last applied push. The full status:

```bash
kubectl -n demo get mimicworkload users -o jsonpath='{.status}' | jq
```

```json
{"discovery": {"endpoints": {"inbound": 2}, "exchanges": 45, "lastPushTime": "2026-11-02T10:00:21Z",
 "pods": 2, "sampleBytes": 41230}}
```

## 8. Clean up

```bash
kill $PF 2>/dev/null
kubectl delete namespace demo demo-client
cd .. && rm -r mimic-demo
```

## Next

- Answer the outbound calls from what was recorded, then load-test this app: [Mock mode](Mock-mode) and
  [Load test](Load-test) reuse `demo/users`, so skip the clean-up until you have read them.
- Hide more header or field names: `spec.discovery.redact` in [Configuration](Configuration).
- Why `GET /users/{users_id}` and not one endpoint per id: [Discovery and redaction](Discovery-and-redaction).
- Nothing shows up: [Troubleshooting](Troubleshooting).
