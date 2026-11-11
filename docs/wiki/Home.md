# mimic

mimic discovers the HTTP APIs that run on a Kubernetes cluster, without touching application code.
A sidecar is injected into your Deployments, observes the traffic that crosses each pod, and the
controller merges what every pod saw into one description per Deployment, stored in Valkey and
readable through a small API.

This wiki documents mimic **0.2.0-alpha.1** (pre-release).

It covers features F0 to F11 and the monitoring work. The wiki is not versioned: it describes the
release it was last published from, and `docs/wiki/` at a release tag documents that release. The reference is
[`docs/SPEC.md`](https://github.com/Thaliph/mock-server/blob/main/docs/SPEC.md); when the wiki and
the spec disagree, the spec wins.

## What it does

- **Injection.** A mutating webhook adds an init container (traffic redirection), a native sidecar
  and, per app image, a trust-bundle init container to the pods you select. No change to your
  manifests or images.
- **Transparent proxy.** HTTP/1.x and HTTP/2 are relayed byte for byte; anything else is an opaque
  TCP tunnel. Access logs and RED metrics for every exchange.
- **TLS interception.** Outbound HTTPS is decrypted with a per-deployment CA that the pod trusts
  automatically, and passed through untouched whenever interception could change what the client or
  the server sees.
- **Discovery.** Inbound exchanges become endpoints (path templates, query/header/body schemas,
  volatile fields, latency histograms, samples), with credentials redacted.
- **Collection.** The controller merges the reports of all pods of a Deployment in Valkey and
  exposes them through a read API and the `MimicWorkload` status.
- **UI.** An OpenAPI 3.1 document and a Swagger UI per deployment, on the read API listener.
- **Egress recording and aggregation.** Outbound calls are recorded per authority, merged by the
  controller, and served to every sidecar.
- **Mock mode.** `spec.mode: mock` answers outbound calls from the recorded egress, and
  `discovery` switches back live.
- **Load tests.** A `MimicLoadTest` replays the discovered endpoints against one deployment while it
  is in mock (off by default).
- **Monitoring.** Scrape objects, a Valkey exporter, an optional bundled Prometheus and Grafana, and
  five dashboards.

## Who it is for

| You are | Start here |
|---|---|
| A **developer** who wants to see what your service exposes | [Quick start](Quick-start), [Reading the API](Reading-the-API), [Discovery and redaction](Discovery-and-redaction) |
| An **ops / platform engineer** who installs, sizes, secures and monitors it | [Install](Install), [Configuration](Configuration), [Operations](Operations), [Security](Security), [Troubleshooting](Troubleshooting) |
| A **contributor** | [Developing](Developing) |

## Map of the pages

| Page | Content |
|---|---|
| [Concepts](Concepts) | Components, data flow, injection, discovery and mock modes |
| [Mock mode](Mock-mode) | Answering outbound calls from the recorded egress, and switching it live |
| [Install](Install) | Requirements, `helm install` of a released version from OCI, verifying images and chart, upgrade, uninstall |
| [Quick start](Quick-start) | Two Python services, discovery, the merged report |
| [Configuration](Configuration) | `MimicWorkload` fields, Helm values, `MIMIC_*` variables |
| [TLS interception](TLS-interception) | How it works, trust injection, exclusions, pass-through |
| [Discovery and redaction](Discovery-and-redaction) | What is recorded, templating, limits, what is never stored |
| [Egress endpoint](Egress-endpoint) | What each sidecar pulls from the controller |
| [Egress aggregation](Egress-aggregation) | Outbound calls per pod, merged by the controller, outbound documents, fallback to older controllers |
| [Reading the API](Reading-the-API) | The read API endpoints and example output |
| [Load test](Load-test) | Load-testing a mocked deployment: the `MimicLoadTest`, safety guarantees, results |
| [Monitoring](Monitoring) | Scrape objects, Valkey exporter, bundled Prometheus and Grafana, dashboards |
| [Operations](Operations) | Metrics, logs, probes, sizing, Valkey, retention, scaling, shutdown |
| [Security](Security) | Threat model summary, what reaches Valkey, RBAC, network, accepted limits |
| [Troubleshooting](Troubleshooting) | Symptom, cause, fix |
| [FAQ](FAQ) | Short answers |
| [Developing](Developing) | Build, test, e2e on kind, repository layout, workflow |

## Release status

`0.2.0-alpha.1` is a pre-release: the `v1alpha1` CRDs may change incompatibly before `0.2.0`, and
there is no upgrade from the legacy `v0.1.0`. See the
[CHANGELOG](https://github.com/Thaliph/mock-server/blob/main/CHANGELOG.md) and the accepted limits in
[Security](Security).

Traces are out of scope; mesh traffic management is a later version.
