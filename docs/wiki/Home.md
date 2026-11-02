# mimic

mimic discovers the HTTP APIs that run on a Kubernetes cluster, without touching application code.
A sidecar is injected into your Deployments, observes the traffic that crosses each pod, and the
controller merges what every pod saw into one description per Deployment, stored in Valkey and
readable through a small API.

This wiki covers what is **merged today (features F0 to F5)**. The reference is
[`docs/SPEC.md`](https://github.com/Thaliph/mock-server/blob/main/docs/SPEC.md); when the wiki and
the spec disagree, the spec wins.

## What it does today

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
| [Install](Install) | Requirements, `helm install` from a clone, upgrade, uninstall, the PVC upgrade note |
| [Quick start](Quick-start) | Two Python services, discovery, the merged report |
| [Configuration](Configuration) | `MimicWorkload` fields, Helm values, `MIMIC_*` variables |
| [TLS interception](TLS-interception) | How it works, trust injection, exclusions, pass-through |
| [Discovery and redaction](Discovery-and-redaction) | What is recorded, templating, limits, what is never stored |
| [Reading the API](Reading-the-API) | The read API endpoints and example output |
| [Operations](Operations) | Metrics, logs, probes, sizing, Valkey, retention, scaling, shutdown |
| [Security](Security) | Threat model summary, what reaches Valkey, RBAC, network, accepted limits |
| [Troubleshooting](Troubleshooting) | Symptom, cause, fix |
| [FAQ](FAQ) | Short answers |
| [Developing](Developing) | Build, test, e2e on kind, repository layout, workflow |

## Roadmap

Planned, **not available yet**; nothing in this wiki depends on it.

- **F6, UI.** OpenAPI 3.1 document and Swagger UI per deployment, on the read API listener.
- **F7 to F9, egress.** Record the calls a deployment makes to external APIs, share them with the
  controller, aggregate them per deployment.
- **F10, mock mode.** Answer a deployment's egress calls from recorded responses, and switch back to
  discovery.
- **F11, load testing** of one workload in isolation.

Traces are out of scope; mesh traffic management is a later version.
