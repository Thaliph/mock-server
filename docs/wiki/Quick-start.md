# Quick start

Audience: developers. You will inject two small Python services, send traffic, and read the API that
mimic discovered. About 10 minutes. Covers F0 to F5.

You need a cluster with mimic installed ([Install](Install)), `kubectl`, `curl`, `jq`, and a clone of
the repository (the services are in `scripts/e2e/apps/`; they are the ones the end-to-end tests use).

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

```bash
kubectl -n demo create configmap apps-code --from-file=service.py=scripts/e2e/apps/service.py
kubectl -n demo apply -f scripts/e2e/apps/users.yaml

kubectl -n demo-client create configmap apps-code --from-file=service.py=scripts/e2e/apps/service.py
sed 's/users\.disc-22\.svc/users.demo.svc/' scripts/e2e/apps/front.yaml | kubectl -n demo-client apply -f -

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
(`MIMIC_STATUS_INTERVAL`). Wait about a minute.

Optional: look at what one pod holds *before* it is shipped. `/discovery` is served on the sidecar
admin port to loopback peers only, so use a port-forward:

```bash
pod=$(kubectl -n demo get pod -l app=users -o jsonpath='{.items[0].metadata.name}')
kubectl -n demo port-forward "pod/$pod" 15100:15100 &
curl -s localhost:15100/discovery | jq '.exchanges'
```

After a push is acknowledged, the shipped counters and samples leave the local report, so this number
drops back to what was recorded since.

## 6. Read the merged report

The read API listens on the loopback of each controller pod (`127.0.0.1:8080`); reach it with a
port-forward (Kubernetes RBAC on `pods/portforward` in the release namespace is the authentication):

```bash
kubectl -n mimic-system port-forward deploy/mimic-controller 8080:8080 &

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
grep -o 'Bearer <redacted>' report.json | sort -u
# Bearer <redacted>
```

## 7. See it in kubectl

```bash
kubectl -n demo get mimicworkload
```

```
NAME    MODE        ENDPOINTS   PODS   LAST PUSH
users   discovery   2           2      25s
```

`ENDPOINTS` is the inbound endpoint count, `PODS` the pods that pushed within the last 5 minutes
(`MIMIC_STATUS_POD_WINDOW`), `LAST PUSH` the age of the last applied push. The full status:

```bash
kubectl -n demo get mimicworkload users -o jsonpath='{.status}' | jq
```

```json
{"discovery": {"endpoints": {"inbound": 2}, "exchanges": 45, "lastPushTime": "2026-11-02T10:00:21Z",
 "pods": 2, "sampleBytes": 41230}}
```

(The status subresource is part of the F5 change that is merging now; on a chart without it, use the
read API.)

## 8. Clean up

```bash
kill %1 %2 2>/dev/null
kubectl delete namespace demo demo-client
```

## Next

- Hide more header or field names: `spec.discovery.redact` in [Configuration](Configuration).
- Why `GET /users/{users_id}` and not one endpoint per id: [Discovery and redaction](Discovery-and-redaction).
- Nothing shows up: [Troubleshooting](Troubleshooting).
