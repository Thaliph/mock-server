# mimic

mimic is for testing a service on its own. In a Kubernetes cluster it records what a service exposes and
which other services it calls, without touching the application code. It then answers those outbound
calls from the recording, so you can load-test the service alone, without its dependencies, and without
calling a real provider.

This wiki documents mimic **0.2.0-alpha.3** (pre-release).

The wiki is not versioned: it describes the
release it was last published from, and `docs/wiki/` at a release tag documents that release. The reference is
[`docs/SPEC.md`](https://github.com/Thaliph/mock-server/blob/main/docs/SPEC.md); when the wiki and
the spec disagree, the spec wins.

## Get started

Five pages, in this order, about 30 minutes in all on a laptop with [kind](Install#try-it-on-kind):

1. [Install](Install): a kind cluster and `helm install` of the released version.
2. [Quick start](Quick-start): two small demo services, injected, with traffic sent through them and the
   discovered reports read back.
3. [Mock mode](Mock-mode): switch one service to answer its outbound calls from the recording, and call it
   with its dependency switched off.
4. [Load test](Load-test): load-test that service, alone, while its dependency is never called.
5. [Reading the API](Reading-the-API): read the results, and clean up.

mimic is a sidecar injected into your Deployments, which observes the traffic that crosses each pod, and a
controller that merges what every pod saw into one description per Deployment, stored in Valkey and
readable through a small API. [Concepts](Concepts) explains the parts.

## What it does

- **Injection.** A mutating webhook adds an init container (traffic redirection), a sidecar (a
  container that runs next to your app for the whole life of the pod; Kubernetes calls this a native
  sidecar) and, per app image, a trust-bundle init container to the pods you select. No change to your
  manifests or images.
- **Transparent proxy.** HTTP/1.x and HTTP/2 are relayed byte for byte; any other protocol is relayed
  as plain bytes without being read (an opaque TCP tunnel). Access logs and request, error and duration
  metrics (the RED metrics) for every exchange.
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
| A **developer** who wants to see what a service exposes and calls, mock its dependencies and load-test it | The five pages of [Get started](#get-started), beginning with [Install](Install#try-it-on-kind) (a kind cluster on your laptop); then [Discovery and redaction](Discovery-and-redaction) |
| An **ops / platform engineer** who installs, sizes, secures and monitors it | [Install](Install) (the whole page), [Configuration](Configuration), [Operations](Operations), [Security](Security), [Troubleshooting](Troubleshooting) |
| A **contributor** | [Developing](Developing) |

## Map of the pages

| Page | Content |
|---|---|
| [Concepts](Concepts) | Components, data flow, injection, discovery and mock modes |
| [Mock mode](Mock-mode) | Answering outbound calls from the recorded egress, and switching it live |
| [Install](Install) | Requirements, trying it on kind (where everyone starts), `helm install` of a released version from OCI, enabling load tests, verifying images and chart, upgrade, uninstall |
| [Quick start](Quick-start) | Two Python services, discovery of both, the merged reports (downloads its demo files; no clone needed) |
| [Configuration](Configuration) | `MimicWorkload` fields, Helm values, `MIMIC_*` variables |
| [TLS interception](TLS-interception) | How it works, trust injection, exclusions, pass-through |
| [Discovery and redaction](Discovery-and-redaction) | What is recorded, templating, limits, what is never stored |
| [Egress endpoint](Egress-endpoint) | What each sidecar pulls from the controller |
| [Egress aggregation](Egress-aggregation) | Outbound calls per pod, merged by the controller, outbound documents, fallback to older controllers |
| [Reading the API](Reading-the-API) | The read API endpoints (reports, OpenAPI documents, load test runs) and example output |
| [Load test](Load-test) | Load-testing a mocked deployment: the `MimicLoadTest`, safety guarantees, results |
| [Monitoring](Monitoring) | Scrape objects, Valkey exporter, bundled Prometheus and Grafana, dashboards |
| [Operations](Operations) | Metrics, logs, probes, sizing, Valkey, retention, scaling, shutdown |
| [Security](Security) | Threat model summary, what reaches Valkey, RBAC, network, accepted limits |
| [Troubleshooting](Troubleshooting) | Symptom, cause, fix |
| [FAQ](FAQ) | Short answers |
| [Developing](Developing) | Build, test, e2e on kind, repository layout, workflow |

## Release status

`0.2.0-alpha.3` is a pre-release: the `v1alpha1` CRDs may change incompatibly before `0.2.0`, and
there is no upgrade from the legacy `v0.1.0`. See the
[CHANGELOG](https://github.com/Thaliph/mock-server/blob/main/CHANGELOG.md) and the accepted limits in
[Security](Security).

For contributors: this release covers the features F0 to F11 of the delivery plan in the spec, and the
monitoring work.

Traces are out of scope; mesh traffic management is a later version.
