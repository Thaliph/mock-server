# Configuration

Three layers: the `MimicWorkload` (per deployment), the Helm values (per installation), and the
`MIMIC_*` environment variables (what the processes read). Configuration is environment variables
only; an invalid or missing required value stops the process at start with **exit code 2** and a
message naming the variable (`MIMIC_X="value": reason` or `MIMIC_X is required`).

Durations are `<n>ms`, `<n>s`, `<n>m`, `<n>h` or `<n>d` where the range says so. Sizes are `<n>`,
`<n>Ki`, `<n>Mi`.

## `MimicWorkload`

`apiVersion: mimic.platform.io/v1alpha1`, namespaced. Settings under `spec.tls` and `spec.discovery`
apply to pods **created afterwards**: run `kubectl rollout restart` after a change. When several
`MimicWorkload`s name the same Deployment, their lists are merged (sorted, deduplicated).

| Field | Type | Default | Constraints | Meaning |
|---|---|---|---|---|
| `spec.deployment` | string | required | DNS-1123 subdomain, max 253, **immutable** | Deployment of the same namespace this resource selects. |
| `spec.mode` | `discovery` \| `mock` | `discovery` | enum | `mock` is planned (F10) and has no effect yet; leave it unset. |
| `spec.tls.exclude` | list of string | empty | max 256 entries, each max 253 chars | Names never TLS-intercepted: exact host, `*.example.com` (below, not itself) or IP literal. See [TLS interception](TLS-interception). A union above 256 entries is cut to the first 256 in sorted order (WARN `tls exclude truncated`). |
| `spec.discovery.redact` | list of string | empty | max 64 entries, each 1-64 chars of `A-Za-z0-9_-` | Extra names redacted by name, lower-cased by the webhook. See [Discovery and redaction](Discovery-and-redaction). |

Status (written by the controller only, through the status subresource; F5):

| Field | Meaning |
|---|---|
| `status.discovery.endpoints.inbound` / `.outbound` | Endpoints per direction; a direction without any is omitted. |
| `status.discovery.exchanges` | Exchanges merged. |
| `status.discovery.sampleBytes` | Bytes held by samples. |
| `status.discovery.pods` | Pods whose last push is within `MIMIC_STATUS_POD_WINDOW`. |
| `status.discovery.lastPushTime` | Last applied push, UTC, seconds; absent without data. |

`kubectl get mimicworkload` prints `Mode`, `Endpoints` (inbound count), `Pods` and `Last push` (age).
A deployment without data has zero counts and no `lastPushTime`.

## Helm values

Chart `oci://ghcr.io/thaliph/charts/mimic`. Defaults from `values.yaml`.

| Value | Default | Range / notes |
|---|---|---|
| `image.repository` | `ghcr.io/thaliph/mimic` | Holds `mimic-controller` and `mimic-sidecar`. |
| `image.tag` | `""` (chart appVersion) | |
| `image.digest` | `""` | `sha256:...`; takes precedence over `tag`. |
| `image.pullPolicy` | `IfNotPresent` | |
| `imagePullSecrets` | `[]` | |
| `controller.replicas` | `2` | Keep 2 or more: the webhook fails closed. |
| `controller.logLevel` | `info` | `MIMIC_LOG` of the controller (`tracing` filter syntax). |
| `controller.resources` | requests `50m` / `64Mi`; limit memory `512Mi` | Sized for two report writes at once plus the push body budget. |
| `controller.nodeSelector`, `controller.tolerations` | `{}`, `[]` | |
| `controller.affinity` | `{}` | Empty: preferred pod anti-affinity on `kubernetes.io/hostname`. |
| `controller.webhook.maxConnections` | `256` | 1-65535. `MIMIC_WEBHOOK_MAX_CONNECTIONS`. |
| `controller.webhook.requestTimeout` | `5s` | `<n>s` or `<n>ms`, greater than 0. |
| `controller.sidecarApiPort` | `8444` | Port of the sidecar API; also given to sidecars. |
| `controller.status.interval` | `30s` | 1s-1h. `MIMIC_STATUS_INTERVAL`. (F5 status.) |
| `controller.status.podWindow` | `5m` | 1s-24h. `MIMIC_STATUS_POD_WINDOW`. (F5 status.) |
| `inject.initImage.repository` | `ghcr.io/thaliph/mimic-init` | |
| `inject.initImage.tag` / `.digest` | `""` / `""` | Tag defaults to the appVersion; digest wins. |
| `inject.sidecarResources` | requests `50m` / `64Mi`; limit memory `128Mi` | Resources of `mimic-sidecar` in injected pods (JSON in `MIMIC_INJECT_SIDECAR_RESOURCES`). |
| `inject.sidecarLogLevel` | `info` | `MIMIC_LOG` given to injected sidecars. |
| `valkey.image.repository` / `.tag` / `.digest` | `docker.io/valkey/valkey` / `9.0-alpine` / pinned digest | Valkey 9.0 or later is required (hash field expiry). |
| `valkey.existingSecret` | `""` | Secret with key `password`. Empty: the chart creates a random one, kept on upgrade and uninstall. |
| `valkey.persistence.enabled` | `true` | `false`: `emptyDir`, store lost on pod restart. |
| `valkey.persistence.size` | `4Gi` | AOF rewrites need about twice the data. Immutable after install, see the [upgrade note](Install#known-issue-upgrading-from-v010-fails-on-the-valkey-pvc-293). |
| `valkey.persistence.storageClass` | `""` | Empty: cluster default. |
| `valkey.resources` | requests `50m` / `64Mi`; limit memory `1Gi` | `maxmemory` = 75 % of the memory limit (a whole quantity such as `1Gi` is required), policy `noeviction`. |
| `valkey.nodeSelector`, `valkey.tolerations` | `{}`, `[]` | |
| `networkPolicy.enabled` | `true` | Valkey: ingress from the controller only, no egress. Controller: see below. |
| `networkPolicy.webhookFrom` | `[]` | `from` peers for port 8443. Empty: any source (the API server's address depends on the cluster). |
| `networkPolicy.metricsNamespaceSelector` | `{}` | Namespaces allowed to scrape port 8081, in addition to the release namespace. |
| `networkPolicy.sidecarApiFrom` | `[]` | `from` peers for the sidecar API port. Empty: any source (injected pods can be anywhere). |
| `metrics.serviceMonitor.enabled` | `false` | Prometheus Operator `ServiceMonitor` for the controller admin port. |
| `metrics.serviceMonitor.interval` | `30s` | |

The chart does **not** expose `MIMIC_STORE_*`, `MIMIC_SIDECAR_API_*`, `MIMIC_UI_ADDR` or the
sidecar's `MIMIC_DISCOVERY_*` tunables: their defaults apply.

## Environment variables

### Common (every component)

| Variable | Default | Meaning |
|---|---|---|
| `MIMIC_LOG` | `info` | Log filter (`tracing` syntax). |
| `MIMIC_ADMIN_ADDR` | sidecar `0.0.0.0:15100`, controller `0.0.0.0:8081` | `/healthz`, `/readyz`, `/metrics`. |
| `MIMIC_SHUTDOWN_GRACE` | `10s` | Maximum drain time after `SIGTERM` (`<n>s` or `<n>ms`). |

### Controller

"Chart" = set by the chart; others keep their default.

| Variable | Default | Range / meaning |
|---|---|---|
| `MIMIC_VALKEY_URL` | required (chart) | `redis://host:port[/db]`, no credentials. |
| `MIMIC_VALKEY_PASSWORD` | empty (chart: from Secret) | |
| `MIMIC_NAMESPACE` | required (chart) | Downward API. |
| `MIMIC_WEBHOOK_ADDR` | `0.0.0.0:8443` | Webhook HTTPS listener. |
| `MIMIC_WEBHOOK_SERVICE` | required (chart) | Service name (certificate SAN). |
| `MIMIC_WEBHOOK_SECRET` | required (chart) | Secret with the webhook CA and certificate. |
| `MIMIC_WEBHOOK_MAX_CONNECTIONS` | `256` | 1-65535. |
| `MIMIC_WEBHOOK_REQUEST_TIMEOUT` | `5s` | `<n>s`/`<n>ms`, > 0. |
| `MIMIC_WEBHOOK_CONFIGURATION` | required (chart) | `MutatingWebhookConfiguration` whose `caBundle` is maintained. |
| `MIMIC_INJECT_SIDECAR_IMAGE` | required (chart) | Sidecar image in injected pods. |
| `MIMIC_INJECT_INIT_IMAGE` | required (chart) | `mimic-init` image. |
| `MIMIC_INJECT_SIDECAR_RESOURCES` | requests `50m`/`64Mi`, limit `128Mi` (JSON) | Sidecar resources. |
| `MIMIC_INJECT_SIDECAR_LOG` | `info` | `MIMIC_LOG` for sidecars. |
| `MIMIC_TLS_CA_SECRET` | required (chart) | Secret holding the TLS seed. |
| `MIMIC_SIDECAR_API_ADDR` | `0.0.0.0:8444` | Sidecar API HTTPS listener. |
| `MIMIC_SIDECAR_API_RATE` | `10` | Requests/s per source address, burst twice that. 1-10000. |
| `MIMIC_SIDECAR_API_MAX_REPORT` | `32Mi` | Largest push body. 64Ki-1Gi. |
| `MIMIC_SIDECAR_API_BODY_BUDGET` | `64Mi` | Push bodies being read at once; at least `MAX_REPORT`, at most 4Gi. |
| `MIMIC_STORE_TTL` | `30d` | Retention after last push / last seen. 10s-365d. |
| `MIMIC_STORE_MAX_ENDPOINTS` | `512` | Per direction and deployment. 1-10000. |
| `MIMIC_STORE_MAX_PUSH_ENDPOINTS` | `256` | Per direction in one push; 1-10000, at most `MAX_ENDPOINTS`. |
| `MIMIC_STORE_MAX_LITERALS` | `32` | Literal limit of the store's templating. 2-1000. |
| `MIMIC_STORE_SAMPLE_BUDGET` | `4Mi` | Sample bytes per deployment. 1Mi-1Gi. |
| `MIMIC_STORE_MAX_DEPLOYMENTS` | `50` | Deployments with data per namespace. 1-100000. |
| `MIMIC_STATUS_INTERVAL` | `30s` | 1s-1h (chart: `controller.status.interval`). |
| `MIMIC_STATUS_POD_WINDOW` | `5m` | 1s-24h (chart: `controller.status.podWindow`). |
| `MIMIC_UI_ADDR` | `127.0.0.1:8080` | Read API; plain HTTP, loopback peers only. |

### Sidecar

Set by the webhook; the defaults are what it sets unless noted.

| Variable | Default | Meaning |
|---|---|---|
| `MIMIC_INBOUND_ADDRS` | `0.0.0.0:15006,[::]:15006` | Inbound listeners. |
| `MIMIC_OUTBOUND_ADDRS` | `127.0.0.1:15001,[::1]:15001` | Outbound listeners, loopback only. |
| `MIMIC_WORKLOAD` | required | Deployment name. |
| `MIMIC_POD_NAMESPACE`, `MIMIC_POD_NAME`, `MIMIC_POD_IP` | required | Downward API. |
| `MIMIC_APP_PROBES` | `{}` | The app's original probes (JSON). |
| `MIMIC_MAX_CONNECTIONS` | `16384` | Per direction. 1-1000000. |
| `MIMIC_METRICS_MAX_AUTHORITIES` | `50` | Distinct `authority` label values per direction and metric; the rest become `(other)`. 1-10000. Not set by the webhook. |
| `MIMIC_TLS_CA` | empty | Deployment root certificate (PEM). Empty: no interception. |
| `MIMIC_TLS_EXCLUDE` | empty | Exclusions, comma-separated; an invalid entry or more than 256 exits 2. |
| `MIMIC_CONTROLLER_URL` | set by the webhook | Sidecar API URL. Without it, reports are not shipped. |
| `MIMIC_CONTROLLER_CA` | set by the webhook | CA the sidecar API certificate is verified against. |
| `MIMIC_TLS_TOKEN_FILE` | `/var/run/secrets/mimic.platform.io/token` | Projected token, re-read per request. |
| `MIMIC_TRUST_DIR` | `/mimic` | Trust volume. |
| `MIMIC_TRUST_BINARY` | `/usr/local/bin/mimic-trust` | Tests only; never set by the webhook. |
| `MIMIC_DISCOVERY_MAX_ENDPOINTS` | `256` | Per direction. 1-10000. Not set by the webhook. |
| `MIMIC_DISCOVERY_MAX_LITERALS` | `32` | Literals below a template node before it becomes a parameter. 2-1000. Not set by the webhook. |
| `MIMIC_DISCOVERY_SAMPLE_BUDGET` | `16Mi` | Bytes held by samples. 1Mi-1Gi. Not set by the webhook. |
| `MIMIC_DISCOVERY_CAPTURE_BUDGET` | `8Mi` | Body bytes captured in flight and queued. 1Mi-1Gi. Not set by the webhook. |
| `MIMIC_DISCOVERY_QUEUE` | `1024` | Records waiting for the recorder. 1-65536. Not set by the webhook. |
| `MIMIC_DISCOVERY_PUSH_INTERVAL` | `30s` | 1s-1h. Not set by the webhook. |
| `MIMIC_DISCOVERY_REDACT_NAMES` | empty | Set by the webhook from `spec.discovery.redact`. At most 64 names. |

### `mimic-init` and `mimic-trust`

| Variable | Default | Meaning |
|---|---|---|
| `MIMIC_INBOUND_PORT` | `15006` | Inbound redirection target. |
| `MIMIC_OUTBOUND_PORT` | `15001` | Outbound redirection target. |
| `MIMIC_PROXY_UID` | `1337` | UID whose traffic is not redirected. |
| `MIMIC_EXCLUDE_INBOUND_PORTS` | empty | Ports (1-65535) never redirected inbound. |
| `MIMIC_TRUST_CA` | required | Root certificate (PEM). |
| `MIMIC_TRUST_OUT` | required | Output directory. |
| `MIMIC_TRUST_FALLBACK` | `/mimic/bin/fallback-ca.pem` | Store used when the image has none. |

Injected app containers also get `SSL_CERT_FILE`, `REQUESTS_CA_BUNDLE`, `CURL_CA_BUNDLE`,
`NODE_EXTRA_CA_CERTS` and `JAVA_TOOL_OPTIONS`; see [TLS interception](TLS-interception).
