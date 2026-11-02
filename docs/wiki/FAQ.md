# FAQ

**Do I have to change my application?**
No. Injection adds containers and, for TLS, environment variables and a CA bundle mount. Only the
`httpGet`, `tcpSocket` and `grpc` probes of your containers are rewritten.

**Which workloads are supported?**
Pods owned by a Deployment. HTTP/1.x and HTTP/2 are understood; other TCP protocols are tunnelled and
logged and measured at connection level. gRPC streaming, WebSocket and SSE are passed through.

**Does mimic slow my traffic or break it?**
It forwards bytes unchanged and never refuses or synthesises a response; discovery is best effort and
never waits. When the sidecar is down, redirected connections are refused (fail-closed). If the
controller is down, new pods in scope cannot be created.

**Is HTTPS visible?**
Outbound HTTPS is, when the client trusts the injected bundle ([TLS interception](TLS-interception)).
Inbound HTTPS is not decrypted, so it is not discovered.

**What about egress (calls my service makes)?**
Not recorded yet (planned, F7 to F9). Today the reports describe what the deployment exposes.

**Can I mock a dependency or load-test?**
Not yet: mock mode (F10) and load testing (F11) are planned. `spec.mode: mock` is accepted but does
nothing.

**Is there a UI or an OpenAPI file?**
Planned (F6). Today use the JSON [read API](Reading-the-API).

**How do I see what was discovered?**
`kubectl port-forward` to a controller pod and `GET /v1/workloads/<ns>/<dep>/report`, or
`kubectl get mimicworkload` for the summary.

**Why `{users_id}` and not `{id}`?**
A parameter is named after the preceding literal segment; positions with no literal before get
`{param<n>}`.

**Are secrets stored?**
Known credential names and forms are replaced by `<redacted>` in the sidecar and again in the
controller. A secret under an unlisted name in an unrecognised form is not caught: add its name to
`spec.discovery.redact`. See [Discovery and redaction](Discovery-and-redaction) and
[Security](Security).

**I changed a `MimicWorkload` and nothing happened.**
Pod-level settings apply to pods created afterwards: `kubectl rollout restart`.

**How long is data kept?**
30 days after the last push (`MIMIC_STORE_TTL`); it can be reset with `valkey-cli DEL`
([Operations](Operations#valkey)).

**Do I need Valkey persistence?**
No, but without a PVC the store is lost when the Valkey pod restarts; sidecars rebuild it from traffic.

**Why do namespaces need the `privileged` Pod Security level?**
`mimic-init` needs `NET_ADMIN` and `NET_RAW` to install the redirection. Only the init container has
them; it exits afterwards.

**Does it work with a service mesh or another iptables tool?**
No: `mimic-init` owns the built-in chains it hooks in the pod's network namespace.

**Does the controller need access to my pods or Secrets?**
No. It reads `MimicWorkload`s and writes their status, and touches only its own two Secrets and its own
webhook configuration ([Security](Security#rbac)).

**Which Kubernetes versions?**
1.33 or later (native sidecars).

**How do I uninstall completely?**
[Install](Install#uninstall): the CRD, the Valkey password Secret and the PVC are kept on purpose.

**Where is the reference?**
`docs/SPEC.md` in the repository.
