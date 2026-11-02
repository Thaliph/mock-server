# mimic

API discovery and dependency mocking for Kubernetes workloads, without touching application code.

- **Document** the HTTP APIs running on a cluster: a sidecar observes each injected deployment's
  ingress and egress, and the controller publishes an OpenAPI description per deployment.
- **Load-test one workload in isolation**: switch a deployment to *mock* mode and its egress calls
  (payment provider, SaaS APIs) are answered from recorded responses with recorded latencies, so
  you can push it to saturation without calling — or being rate-limited by — its dependencies.

> Status: v2 rewrite in progress. Delivered: **F0 foundations** (runtime, image, Helm chart with
> Valkey, CI/CD). Plan and acceptance criteria: [`docs/SPEC.md`](docs/SPEC.md) §12. The previous
> implementation is kept under the tag `legacy-v0.37`.

## Install (development)

Requirements: Kubernetes ≥ 1.33, Helm 3.8+ (OCI support).

Injected apps must exit on `SIGTERM`: the sidecar ships its last discovery report only in the
part of the grace period the app leaves it (a loss is logged as `report not shipped`).

```bash
helm install mimic oci://ghcr.io/thaliph/charts/mimic -n mimic-system --create-namespace
kubectl -n mimic-system port-forward svc/mimic-controller 8081:8081 &
curl -s localhost:8081/readyz
```

## Develop

```bash
cargo test --workspace     # unit and integration tests
scripts/e2e.sh             # build the image and test the chart on a local kind cluster
```

Components: `mimic-core` (pure logic), `mimic-runtime` (configuration, logs, metrics, health,
shutdown), `mimic-sidecar`, `mimic-controller`. Configuration is environment variables only
(`MIMIC_*`, see spec §10).

## License

Apache-2.0
