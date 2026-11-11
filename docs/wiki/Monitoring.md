# Monitoring

mimic exposes Prometheus
metrics on its admin port, and the chart can scrape them. A **bundled Prometheus and Grafana** are an option for a
demo or a dev cluster. A production install brings its own monitoring and uses the ServiceMonitor and the
PodMonitors instead.

## Scrape objects of a production monitoring stack

| Value | Default | What it renders |
|---|---|---|
| `metrics.serviceMonitor.enabled` | `false` | A ServiceMonitor `<release>-controller` for the controller's admin port (8081). Needs the `monitoring.coreos.com` CRDs. |
| `metrics.podMonitor.enabled` | `false` | PodMonitors `<release>-sidecar` (the injected pods, port `mimic-admin` 15100) and `<release>-load` (the load generators). |
| `valkey.metrics.enabled` | `false` | The Redis exporter next to Valkey (port 9121, a read-only ACL user). Enabling it restarts Valkey once. |

All three are off by default, so the default render is unchanged.

The PodMonitors select pods in **every namespace** by default (`metrics.podMonitor.namespaceSelector: {any: true}`),
since a pod can be injected through a `MimicWorkload` alone. The Prometheus that reads them needs `list` and `watch`
on `pods` in each namespace with injected pods (a ClusterRole, or a Role per namespace). Without that RBAC the
namespace has no target and Prometheus logs forbidden errors. To narrow the scrape, set the selector with
`matchNames` (or `matchLabels`) to the namespaces that run injected pods.

## The bundled Prometheus and Grafana

For a demo or a dev cluster only. Both are off by default and rendered by the chart's own templates: there is no
Helm dependency, and nothing is vendored into the chart.

```bash
helm upgrade --install mimic deploy/helm/mimic -n mimic-system \
  --set monitoring.prometheus.enabled=true \
  --set monitoring.grafana.enabled=true \
  --set valkey.metrics.enabled=true
```

- **Prometheus** (`<release>-prometheus`): one replica, 24 h of data in an emptyDir of 1 Gi, with no
  persistent volume. Its scrape jobs are the same targets as the ServiceMonitor and the PodMonitors: the
  controller, the injected sidecars, the load generators and, with `valkey.metrics.enabled`, the Valkey
  exporter. It reads pods only (`list` and `watch`), in the whole cluster or in `monitoring.prometheus.namespaces`.
  It has no admin, lifecycle or remote-write endpoint.
- **Grafana** (`<release>-grafana`): anonymous access is off, analytics, update checks and plugins are off, and
  nothing calls the internet. The datasource `mimic-prometheus` is provisioned. Dashboards are read from the
  ConfigMaps of the release namespace that carry the label `grafana_dashboard: "1"`. The dashboards themselves
  come with #380 to #384.
- **Admin password**: the Secret `<release>-grafana-admin` holds it (`admin-password`, 32 characters, kept across
  upgrades). Read it with:

  ```bash
  kubectl -n mimic-system get secret mimic-grafana-admin -o jsonpath='{.data.admin-password}' | base64 -d; echo
  ```

- **Services**: both are `ClusterIP`, and no ingress is rendered. Reach Grafana with a port-forward:

  ```bash
  kubectl -n mimic-system port-forward svc/mimic-grafana 3000:3000
  ```

  Then open `http://localhost:3000`, log in as `admin` with the password above.

## Dashboards

Five dashboards ship with the chart in `deploy/helm/mimic/dashboards/`: `mimic-controller`, `mimic-sidecar`
(traffic), `mimic-discovery`, `mimic-mock-load` (mock mode and load tests) and `mimic-valkey`. With
`monitoring.grafana.enabled`, or `monitoring.dashboards.enabled` for your own Grafana, the chart renders one
ConfigMap per file, labelled `grafana_dashboard: "1"`; `monitoring.dashboards.annotations` is added to each
(for example `grafana_folder: Mimic`). They are tagged `mimic` and read the metrics listed above.

## Network and an external datasource

With `networkPolicy.enabled` (the default), Prometheus admits queries from the Grafana pods only, and Grafana
admits the release namespace only. Prometheus reaches the scrape ports, the controller and the Valkey exporter.
Grafana reaches Prometheus, DNS and the API server (its dashboard sidecar watches the ConfigMaps). A port-forward
is not blocked by a NetworkPolicy, and kind's default CNI does not enforce NetworkPolicy. Set
`networkPolicy.apiServerCIDR` to narrow the API server egress.

To use a Prometheus outside the release, set `monitoring.grafana.datasourceUrl` **and**
`monitoring.grafana.datasourceEgress`. The second is a list of NetworkPolicy egress rules. Without it, Grafana
cannot reach the external datasource:

```yaml
monitoring:
  grafana:
    datasourceUrl: https://prometheus.example.internal:9090
    datasourceEgress:
      - to: [{ipBlock: {cidr: 10.20.0.0/16}}]
        ports: [{port: 9090, protocol: TCP}]
```

## Limits

- The bundled Prometheus reads pod objects, which show literal env values to it. This is accepted for a demo
  (an accepted limit, see [Security](Security#accepted-limits)). It does not scrape the kubelet or cAdvisor, which need `nodes/proxy`: use the `process_*` series.
- The Prometheus data are lost on a pod restart. Grafana's database is in an emptyDir as well.
- Alerting, long-term storage and ingress are out of scope.
