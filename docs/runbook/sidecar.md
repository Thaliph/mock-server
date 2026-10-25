# Sidecar runbook

[Back to the guide](../guide/README.md) · Controller messages: [controller runbook](controller.md) ·
French troubleshooting by symptom: [Dépannage](../guide/Depannage.md)

Every message the `mimic` sidecar (container `mimic`, crate `mimic-proxy`) shows an operator,
with its cause in a real cluster, its effect on the application and its fix. Messages are in
French, **verbatim** as the code emits them; `<…>` marks a substituted value and `{}` a
placeholder written exactly as the extraction test reads it. The inventory with source lines is
`docs/revue/inventaire-erreurs.md` section 2.

A test (`DOC-12`, `crates/mimic-proxy/tests/runbook_doc.rs`) fails when a message of the
sidecar code has no row here with a cause, an effect and a fix.

## How to read the page

- **Level**: `exit 1` / `exit 2` is a message on stderr prefixed `mimic: ` followed by process
  exit (the container goes to `CrashLoopBackOff`); `WARN`, `ERROR` is a JSON log line, the text
  being in `fields.message`; `HTTP NNN` is the body of a response the sidecar fabricates for the
  application (statuses `599`, `502`, `504`, `413`, `403` are explained from the application
  side in [Développeur applicatif](../guide/Developpeur-applicatif.md#réponses-fabriquées-par-le-sidecar));
  `probe` is the output of the `exec` probe (`mimic probe`), seen in pod events.
- Exit codes: configuration errors (`Config::load_with`) exit with code **2**, errors raised
  while starting (`start()`) exit with code **1**.
- **Effect on the application**: the sidecar is on the data path of the pod. A start-up error
  means the pod never becomes ready; and because the init container already installed the
  iptables rules, a *crashing* sidecar cuts the traffic of the pod until it restarts. A runtime
  warning almost never affects the application: it concerns the control plane (mode, snapshot)
  or a degraded copy of the traffic. The effect column says which.
- Rate limits: repeating warnings are logged at most once per second (once per 30 s for the
  heartbeat) with an `omitted` field counting the lines dropped.
- Restart: a sidecar restart takes the container start time plus, under the controller, the
  start-up barrier (`share.startup.timeout`, 30 s by default, at most 300 s) during which the
  application container is held at `Init`. Exchanges still in memory and not yet written to the
  local journal are lost; the journal in `store.dir` survives a container restart (not a pod
  recreation).

First commands:

```sh
kubectl -n <ns> get pod <pod> -o jsonpath='{.status.containerStatuses[?(@.name=="mimic")]}'
kubectl -n <ns> logs <pod> -c mimic | grep -v '"INFO"'
kubectl -n <ns> logs <pod> -c mimic --previous          # the last words before a crash
kubectl -n <ns> port-forward pod/<pod> 15100 &
curl -s localhost:15100/api/status                      # controller.last_error, share.last_error
curl -s localhost:15100/readyz                          # waiting, aligned, last-known or fallback
```

## 1. Startup and configuration

The sidecar reads the ConfigMap key `mimic.platform.io/config` (file `--config` or
`MIMIC_CONFIG`), then the environment set by the webhook. A wrong value never leaves a
half-started sidecar: the process exits and the pod stays out of service. Rows exit with code 2
unless stated.

| Message | Level | Cause | Effect on the application | Fix |
|---|---|---|---|---|
| `mimic: lecture de <fichier>: <err>` | exit 2 | configuration file unreadable: ConfigMap not mounted or wrong key | pod not ready, application held at `Init` while it waits for the sidecar | check the volume: `kubectl -n <ns> describe pod <pod>` and `kubectl -n <ns> get cm` |
| `mimic: configuration {} invalide: {}` | exit 2 | YAML typo, unknown field or unknown variant (`share.startup.fallback`, `mock.tcp`, `mock.latency` instead of `mock.latency_factor`) | same | fix the ConfigMap, then `kubectl -n <ns> rollout restart deploy/<workload>` |
| `mimic: MIMIC_CONFIG_YAML invalide: <err>` | exit 2 | `sidecar_config` of the webhook ConfigMap `mimic-injector` is invalid (the webhook also validates it at startup) | every new pod crash-loops its sidecar | `kubectl -n mimic-system edit cm mimic-injector`, restart the webhook, recreate the pods |
| `mimic: MIMIC_MODE inconnu: <m>` | exit 2 | variable edited by hand in the pod | pod not ready | use `passthrough`, `record`, `mock` or `mock-partial`; recreate the pod |
| `mimic: MIMIC_STORE_SIZE_LIMIT_BYTES invalide (octets) : <v>` | exit 2 | non-integer value such as `512Mi` (variable set by hand instead of the webhook's resourceFieldRef) | pod not ready | an integer number of bytes, for example `536870912` |
| `mimic: mock.from (mock local) et share sont incompatibles : le mock local se passe de contrôleur` | exit 2 | `--mock-from` or `MIMIC_MOCK_FROM` set in an injected pod that also has `share` | pod not ready | remove one of the two |
| `mimic: redact.salt requis sans contrôleur : fournir MIMIC_REDACT_SALT (Secret <workload>-mimic, clé redact-salt) ou redact.salt` | exit 2 | no salt, no local mock and no https `share`: sidecar outside the controller without the Secret `<workload>-mimic` | pod not ready | `kubectl -n <ns> create secret generic <workload>-mimic --from-literal=redact-salt=$(openssl rand -hex 16)`, or run under the controller |
| `mimic: mock.latency_factor doit être entre 0 et 100` | exit 2 | value out of bounds | pod not ready | set a value between 0 and 100 |
| `mimic: capture.max_exchanges doit être positif` | exit 2 | value is 0 | pod not ready | set a value of at least 1 |
| `mimic: capture.max_store_bytes doit être positif` | exit 2 | value is 0 | pod not ready | set a value greater than 0 |
| `mimic: store.segment_bytes et store.max_segments doivent être positifs` | exit 2 | one of the two is 0 | pod not ready | set both greater than 0 |
| `mimic: store.max_cached_snapshots doit être positif` | exit 2 | value is 0 | pod not ready | set a value of at least 1 |
| `mimic: <nom> doit être une durée positive (en secondes)` | exit 2 | a null duration in `outbound.connect_timeout`, `server.header_read_timeout`, `share.interval`, `share.mode_poll` or `share.startup.timeout` | pod not ready | set a duration greater than 0 |
| `mimic: <nom> doit être positif` | exit 2 | a null value in `server.max_connections`, `capture.max_inflight_bytes` or `share.max_snapshot_bytes` | pod not ready | set a value greater than 0 |
| `mimic: share.startup.timeout ne peut dépasser 300 s : la startupProbe du sidecar ne l'attendrait pas` | exit 2 | timeout above 300 s | pod not ready | set `share.startup.timeout` to 300 or less |
| `mimic: server.max_connections ne peut dépasser <n>` | exit 2 | value above the semaphore maximum | pod not ready | lower `server.max_connections` |
| `mimic: share exige un nom de workload (MIMIC_WORKLOAD)` | exit 2 | `share` set while the workload name is empty | pod not ready | set the annotation `mimic.platform.io/workload` or the label `app.kubernetes.io/name` on the pod template |
| `mimic: share.url non supportée: <url>` | exit 2 | scheme other than `https`, `http`, `file`, `s3` | pod not ready | fix `share.url` (`https://mimic-controller.mimic-system.svc:8443`) |
| `mimic: share.url en http:// refusée : le contrôleur doit être joint en HTTPS (share.allow_insecure pour les tests)` | exit 2 | `http://` URL without `share.allow_insecure` | pod not ready | use `https://`; `allow_insecure` is for tests only |
| `mimic: share.ca_file requis : seule la CA du contrôleur fait foi` | exit 2 | `https://` URL without `share.ca_file` | pod not ready | mount the ConfigMap `mimic-controller-ca` and set `share.ca_file` (the webhook does it) |
| `mimic: share.retention ignoré : la rétention est celle du contrôleur` | WARN at startup | `share.retention` is set in the sidecar ConfigMap; retention is now decided by the controller only | none; the setting has no effect, the sidecar starts | remove `share.retention` from the ConfigMap (set retention on the controller side) |
| `mimic: share.url {} refusée : un sidecar ne partage que par le contrôleur (share.url: https://mimic-controller.mimic-system:8443, avec share.ca_file) ; le partage par stockage objet (s3://, file://) a été retiré. Sans cluster, servir un snapshot avec mimic-proxy --mock-from <fichier>` | exit 2 | `share.url` uses `s3://` or `file://`: direct object-store sharing was removed (#202) | pod not ready, restart loop | set `share.url: https://mimic-controller.mimic-system:8443` with `share.ca_file`; without a cluster run `mimic-proxy --mock-from <file>` |
| `mimic: instance invalide: <instance>` | exit 2 | `POD_NAME` with characters outside letters, digits, `-`, `.`, `_` | pod not ready | restore `POD_NAME` from `metadata.name` in the pod |
| `mimic: tls.ca_cert_file et tls.ca_key_file vont ensemble` | exit 1 | only one of the two interception CA files was given | pod not ready | give both, or neither |
| `mimic: <fichier>: <err io>` | exit 1 | interception CA, `tls.upstream_ca_file` or `share.ca_file` unreadable: ConfigMap `mimic-controller-ca` without the expected key, CA Secret absent | pod not ready | `kubectl -n <ns> get cm mimic-controller-ca -o yaml`; check trust-manager and the volumes |
| `mimic: <fichier>: aucun certificat` | exit 1 | `share.ca_file` holds no PEM: ConfigMap `mimic-controller-ca` published empty (trust-manager failing) | a sidecar under the controller cannot start; pod not ready | `kubectl -n <ns> get cm mimic-controller-ca -o yaml`; read the trust-manager logs |
| `mimic: certificat de CA vide` | exit 1 | the CA PEM file is empty or holds no certificate | pod not ready | provide a PEM CA certificate (and an unencrypted PKCS#8 key) |
| `mimic: <err rcgen ou rustls>` | exit 1 | invalid CA or additional CA PEM: encrypted key, unexpected PKCS#1 format | pod not ready | provide an unencrypted PKCS#8 PEM |
| `mimic: <dir>: <err>` | exit 1 | `ca.crt` or `ca-bundle.crt` cannot be written to `tls.ca_dir`: `emptyDir` `/var/run/mimic/ca` missing or read-only root | pod not ready | check the volumes injected by the webhook: `kubectl -n <ns> get pod <pod> -o yaml` |
| `mimic: snapshot {}: {}` | exit 1 | `--mock-from` file unreadable or invalid (for example `ligne <n>: …`) | the local mock does not start | redo `mimicctl pull` and give the produced file |
| `mimic: store {}: {}` | exit 1 | local journal cannot be created or replayed: `emptyDir` `mimic-store` not mounted, wrong permission (UID 1338), unreadable segment | pod not ready | check the volume; as a last resort recreate the pod to get a fresh `emptyDir` |
| `mimic: écoute {} sur {}: {}` | exit 1 | bind failed; the placeholders are the listener (`Inbound`, `Outbound` or `Admin`), the address and the OS error, for example `Address already in use` | pod not ready | the application or another sidecar uses 15106, 15101 or 15100: change the ports |

## 2. Controller link and heartbeat

The sidecar heartbeats the controller every `share.interval` and applies the desired state it
gets back. Every error below is also in `controller.last_error` or `share.last_error`
(`/api/status`), and the sidecar retries at the next cycle. **The application is not
affected**: the mode and the snapshot simply stop following the controller (the last applied
state is kept) and captured exchanges queue up, then are dropped once the queue is full
(`mimic_capture_dropped_total{reason="share_backlog"}`). Alerts: `MimicHeartbeatsFailing`,
`MimicControllerUnreachableFromSidecars`. Causes on the controller side are in the
[controller runbook](controller.md#2-sidecar-api-and-ingestion).

| Message | Level | Cause | Effect on the application | Fix |
|---|---|---|---|---|
| `battement vers le contrôleur échoué` | WARN, at most once per 30 s, fields `error` and `omitted`; `mimic_heartbeats_total{result="error"}` | the heartbeat got no answer (`Failed`) or the state was received but not fully applied (`Incomplete`); the detail is in the rows below | none; mode and snapshot frozen | read `error` and use the rows below; `curl -s localhost:15100/api/status` after a port-forward |
| `synchronisation du partage échouée` | WARN at each failing cycle; `mimic_share_sync_total{result="error"}` | publishing segments to the controller (or to the object store) failed | none; exchanges queue, then are dropped when the queue is full | read the `error` field; the rows below apply |
| `jeton {}: {}` | in `error` | projected token file unreadable: volume `mimic-token` missing | none; the sidecar cannot authenticate | check the projected volume and `token_audience` (`mimic-controller`) in the pod |
| `<METHODE> <chemin>: délai dépassé` | in `error` | no answer within the timeout: NetworkPolicy, Service without endpoint, controller stopped | none; frozen state | `kubectl -n mimic-system get endpoints mimic-controller`; the pod must carry `mimic.platform.io/sidecar=true` for the NetworkPolicy |
| `<METHODE> <chemin>: <chaîne d'erreurs hyper/rustls>` | in `error`; for example `dns error`, `Connection refused`, `invalid peer certificate: UnknownIssuer`, `BadSignature`, `Expired` | connection, TLS or URL error: Service DNS, sidecar CA different from the issuer CA (old or foreign `mimic-controller-ca`), controller certificate expired | none; frozen state | `kubectl -n mimic-system get certificate mimic-controller-tls`; for a CA rotation follow [certificates](../guide/Exploiter-en-production.md#certificats) |
| `<METHODE> <chemin>: <code> <motif> <body>` | in `error`, body cut at 200 bytes | the controller answered something other than 200 or 201 (401 token, 403 other pod, 503 passive replica or storage unavailable) | none; frozen state | see the controller runbook; the usual cases are in [Dépannage](../guide/Depannage.md#lien-sidecar--contrôleur) |
| `<METHODE> <chemin>: réponse au-delà de share.max_snapshot_bytes (<n> octets)` | in `error` (`snapshot <id>: …`) | compressed mock shape or full snapshot larger than `share.max_snapshot_bytes` | the pod keeps mocking from its local data, `Aligned=False PodsDiverging` | raise `share.max_snapshot_bytes` in the ConfigMap, or reduce the snapshot (`snapshot.max_per_key`) |
| `battement: {}` | in `error` | the 200 answer is not the expected JSON: intermediate HTTP proxy answering a page, incompatible versions | none; frozen state | check `share.url` and the versions of sidecar and controller (alert `MimicVersionSkew`) |
| `snapshot {}: {}` | in `error` (`snapshot <id>: <cause>`) | the snapshot download failed; the cause follows the id | the pod serves its local data and retries | see the next rows |
| `forme de mock du snapshot` | in `error`: `forme de mock du snapshot <id>: illisible en <encodage>, ou au-delà de share.max_snapshot_bytes (<n> octets) une fois décompressée` | decompression failed or the result is too large | the pod stays on its local data | raise `share.max_snapshot_bytes` |
| `forme de mock du snapshot <id> altérée (empreinte <x>, ETag <y>)` | in `error` | received content differs from its fingerprint: intermediate proxy, corruption | retried at the next heartbeat | none if transient; otherwise bypass the proxy |
| `mode souhaité inconnu:` | in `error`, state `Incomplete` | the controller sent a mode this sidecar does not know (controller more recent) | the rest of the state is applied; the mode is kept | update the sidecars: roll the pods to the current image |
| `capture de l'entrant inconnue:` | in `error`, state `Incomplete` | same, for the inbound capture setting | same | update the sidecars |
| `réglage WebSocket inconnu:` | in `error`, state `Incomplete` | same, for the WebSocket setting | same | update the sidecars |
| `état souhaité non gardé sur disque` | WARN, field `error` | `controller/desired.json` or the mock shape cannot be written in `store.dir`: `emptyDir` full or missing | none now; at the next container restart the pod waits for the controller (barrier) instead of resuming its last state | free or enlarge the `emptyDir` (`store.size_limit_bytes`); see `MimicStoreDiskHigh` |
| `contrôleur injoignable au démarrage : repli share.startup.fallback appliqué, l'application démarre` | WARN; `mimic_startup{state="fallback"}` | deadline reached with no known state; the pod runs in its fallback (`record` by default) | the application starts; if its workload is in mock, it may have reached the real dependencies | restore the controller, then recreate the pod; alert `MimicStartedWithoutDesiredState` |
| `pas aligné à l'échéance du démarrage : dernier état souhaité connu appliqué, l'application démarre` | WARN; `mimic_startup{state="last-known"}` | a state was received (or reloaded) but the snapshot was not downloaded at the deadline | the application starts on the last known state | read `last_error` (snapshot rows above) |
| `contrôleur injoignable au démarrage avec share.startup.fallback: fail : l'application attend sa réponse` | ERROR; `/readyz` answers 503, pod stuck at `Init` | deadline reached with `fail` and no known state | the application does **not** start until the controller answers | restore the controller (alert `MimicStartupBlocked`); choose another fallback if blocking is not wanted |
| `sidecar piloté par le contrôleur : basculer via la ressource MimicWorkload` | HTTP 403 on the admin port | `PUT /mode` or `DELETE /store` sent to a sidecar driven by the controller | none; request refused | `mimicctl mode <workload> <mode> -n <ns>`, or edit the `MimicWorkload` |
| `annotation de mode invalide` | WARN, field `value` | annotation `mimic.platform.io/mode` invalid, on a sidecar **not** driven by the controller | none; mode unchanged | `passthrough`, `record`, `mock` or `mock-partial` |
| `partage désactivé` | HTTP 404 on the admin port | a `share` route called on a sidecar without `share` | none | nothing to fix: this sidecar has no controller |

## 3. Capture and store

| Message | Level | Cause | Effect on the application | Fix |
|---|---|---|---|---|
| `écriture du journal local échouée` | WARN, fields `dir` and `omitted` | write, rotation or deletion of a journal segment failed: `emptyDir` full (`sizeLimit`) or read-only filesystem | none now; local copy of the exchanges degraded (`mimic_capture_dropped_total{reason="local_backlog"}`); the pod is evicted when the `emptyDir` limit is hit | alert `MimicStoreDiskHigh`; raise `store.size_limit_bytes` or lower `store.max_segments`; see [Collecte et stockage](../guide/Collecte-et-stockage.md) |
| `lecture de {key}` | in `synchronisation du partage échouée` | `lecture de <clé>: <err>`: object unreadable in a peer-to-peer share (`file://` or `s3://`, outside the controller) | none; sharing delayed | check S3 rights, region and the shared volume; retried at the next cycle |
| `URL de partage non supportée: {}` | in `synchronisation du partage échouée` (`error` field) | direct object-store sharing: `share.url` has a scheme that is neither `file://` nor `s3://`. This mode is refused at startup since #202 | none; sharing never starts, exchanges queue then are dropped | use the controller: `share.url: https://mimic-controller.mimic-system.svc:8443`; for a local mock without controller use `--mock-from <file>` |
| `bucket {}: {}` | in `synchronisation du partage échouée` (`error` field) | `s3://` share: the S3 client cannot be built (bucket name, region, `AWS_*` variables or IRSA missing). Direct object-store sharing is refused at startup since #202 | none; sharing delayed, exchanges dropped once the queue is full | switch to the controller `https://` URL; meanwhile check `AWS_REGION` and the IRSA role of the service account |
| `liste du partage:` | in `synchronisation du partage échouée` (`error` field) | `liste du partage: <err>`: listing of the shared store failed (rights, network, volume gone). Direct object-store sharing is refused at startup since #202 | none; peer data not refreshed, the mock keeps its last data | switch to the controller `https://` URL, or `--mock-from <file>` for a local mock; meanwhile check S3 `ListBucket` rights and the volume |
| `publication de` | in `synchronisation du partage échouée` (`error` field) | `publication de <chemin>: <err>`: writing a segment to the shared store failed (rights, quota, full volume). Direct object-store sharing is refused at startup since #202 | none; the segment stays queued, then exchanges are dropped (`share_backlog`) | switch to the controller `https://` URL; meanwhile check `PutObject` rights and free space |
| `répertoire de partage` | in `synchronisation du partage échouée` (`error` field) | `répertoire de partage <dir>: <err>`: the `file://` directory cannot be created or opened (read-only mount, wrong path). Refused at startup since #202 | none; sharing never starts | use the controller `https://` URL, or `--mock-from <file>`; meanwhile fix the volume mount and its rights |
| `introuvable` | HTTP 404 on the admin port | unknown admin route (typo in the path after a `kubectl port-forward`) | none | check the path; `GET /api/status` is a safe test |
| `sérialisation JSON` | HTTP 500 on the admin port (body is the serde error) | the sidecar could not serialize its own response; not expected | none; the request fails | open a ticket with the body and the sidecar version |
| `cannot convert float seconds to Duration` | exit 2, inside `mimic: configuration {} invalide: {}` | a duration in the ConfigMap is negative, NaN or too large (for example `outbound.connect_timeout: -1`) | pod not ready | set a positive finite number of seconds, then `kubectl -n <ns> rollout restart deploy/<workload>` |
| `body trop gros` | HTTP 413 on the admin port | admin request body above the limit | none; request refused | send a smaller body |
| `mode inconnu (passthrough, record, mock, mock-partial)` | HTTP 400 on the admin port | `PUT /mode` with an unknown mode | none | use one of the four modes |
| `direction: inbound ou outbound` | HTTP 400 on the admin port | wrong `direction` query | none | `inbound` or `outbound` |
| `dépendance inconnue` | HTTP 404 on the admin port | the requested dependency is not in the discovery | none | `mimicctl report <workload> -n <ns>` lists the dependencies |
| `admin mimic : route réservée à la boucle locale (kubectl port-forward)` | HTTP 403 | admin call coming from another pod or from the pod IP (only `/healthz`, `/readyz` and GET `/metrics` are open) | none | `kubectl -n <ns> port-forward pod/<pod> 15100`, then `curl localhost:15100/...` |

Capture drops carry no log line: they are counted in `mimic_capture_dropped_total{reason}`
(`share_backlog`, `local_backlog`, `store_budget`, `inflight_budget`, `truncated`,
`sample_limit`, `no_salt`, `unredactable`; see [Métriques](../guide/Metriques.md)). A
`no_salt` drop means exchanges are abandoned because no redaction salt is known yet: nothing
unredacted reaches the disk.

## 4. Mock

In `mock` and `mock-partial` the sidecar answers the application itself. The `mimic: ` prefix
tells a fabricated answer from a real one; the guide explains each status for the developer.

| Message | Level | Cause | Effect on the application | Fix |
|---|---|---|---|---|
| `mimic: aucun enregistrement ne correspond à cette requête` | HTTP 599, header `x-mimic-unmatched: true`; `mimic_mock_responses_total{match="unmatched"}` | no recorded exchange matches: call never recorded, body not captured (not JSON), HTTPS not intercepted | the call fails with 599 | record more, use `mock-partial`, add the host to `tls-intercept`, `capture-raw-bodies`; `mimicctl report <workload> -n <ns>`; alert `MimicMockUnmatched` |
| `mock : requête sans réponse enregistrée` | WARN, at most once per second; fields `method`, `authority`, `path`, `trace_id`, `status`=599 | log side of the previous row | same | same; the line names the request |
| `mimic: body trop gros pour le mode mock` | HTTP 413; `mimic_mock_responses_total{match="too_large"}` | request body above `capture.max_body_bytes` in mock | the call fails with 413 | raise `capture.max_body_bytes` |
| `mock : body de requête trop gros` | WARN, at most once per second, `status`=413 | log side of the previous row | same | same |
| `mimic: WebSocket sortant refusé en mock (mock.websocket: refuse)` | HTTP 503, header `x-mimic-websocket: refused`; `mimic_websocket_connections{result="refused"}` | outbound WebSocket handshake in mock with `refuse` (intended) | the WebSocket connection fails | `mock.websocket: relay` if the real server must be reached |
| `mimic: tunnel refusé en mode mock (TLS non intercepté)` | HTTP 403 on CONNECT | explicit proxy, host not intercepted, mode `mock` | the HTTPS call fails | add the host to `mimic.platform.io/tls-intercept` and record again, or exclude it |
| `connexion TLS sortante refusée en mode mock (hôte hors de tls.intercept)` | WARN, once per second, fields `destination` and `kind`; `mimic_passthrough_connections_total{kind="tls",result="refused"}` | outbound TLS to a host that is not intercepted, in mock | connection closed without a response (reset or EOF) | intercept the host or exclude it (`exclude-outbound-cidrs`); alert `MimicMockTlsRefused` |
| `connexion TCP sortante refusée en mode mock (mock.tcp: refuse)` | WARN, once per second; `mimic_passthrough_connections_total{kind="tcp",result="refused"}` | database, broker or cache connection in mock with the default `mock.tcp: refuse` | connection closed without a response | exclude the ports (`mimic.platform.io/exclude-outbound-ports`), or `mock.tcp: relay`; alert `MimicMockTcpRefused` |
| `connexion sortante muette refusée en mode mock (mock.tcp: refuse)` | WARN, once per second, after 30 s | a connection where the server talks first (MySQL, SMTP) | the connect hangs 30 s, then closes | exclude those ports |

## 5. TLS interception

| Message | Level | Cause | Effect on the application | Fix |
|---|---|---|---|---|
| `poignée de main TLS refusée par l'application` | WARN, field `authority`; `mimic_tls_handshake_failures_total{reason="client_rejected"}` | the application rejects the forged certificate: mimic CA missing from its trust store (JVM), pinning | the HTTPS call fails on the application side; nothing is captured for that host | add `/var/run/mimic/ca/ca.crt` to the store, or remove the host from `tls-intercept`; alert `MimicTlsInterceptRejected` |
| `certificat forgé impossible` | WARN, field `name`; handshake refused | the CA cannot sign for this SNI (IP address, wildcard) | the HTTPS call fails | remove the host from `tls.intercept` |
| `tunnel vers l'amont impossible` | WARN, once per second; `mimic_upstream_errors_total{kind="connect"}` or `connect_timeout` | CONNECT relayed but the upstream connection fails: dependency unreachable, DNS, NetworkPolicy | the client already got `200` to CONNECT, then the connection is cut | handle it as an outage of the dependency |
| `tunnel impossible` | WARN, once per second, fields `target` and `error`; `mimic_passthrough_connections_total{result="error"}` | opaque TCP relay (non-HTTP) cannot connect: database or broker down; on inbound, the application does not listen on that port | connection closed | check the dependency, or `inbound-ports` |
| `mimic: CONNECT sans destination` | HTTP 400 | the client sent CONNECT without a target | the call fails | fix the proxy settings of the client |

## 6. Connections and limits

| Message | Level | Cause | Effect on the application | Fix |
|---|---|---|---|---|
| `plafond de connexions simultanées atteint, connexion refusée` | WARN, once per second, fields `listener` and `omitted`; `mimic_connections_refused_total{listener}` | more than `server.max_connections` connections (or the admin ceiling): load test, leaking connection pool, keep-alive | the connection is closed **without a response**; the client sees a reset | raise `server.max_connections` or reduce concurrency |
| `accept échoué, écoute suspendue 50 ms` | WARN, once per second, field `omitted` | `accept()` failed, typically `EMFILE` under load | the listener pauses 50 ms; new connections wait | raise the file limit (`ulimit`), lower `server.max_connections` |
| `échec vers l'amont` | WARN, once per second, fields `authority`, `error`, `omitted`; `mimic_upstream_errors_total{direction,kind}` | a request to the upstream failed | HTTP 502 or 504 (next two rows) | see the next rows; alert `MimicInboundUpstreamErrors` for the inbound side |
| `mimic: amont injoignable` | HTTP 502; `kind="connect"` or `kind="request"` | connection or request error towards the upstream: dependency down, application not ready, enterprise CA (TLS) | the call fails with 502 | `tls.upstream_ca_file` for an enterprise CA; wait for the application readiness; otherwise as without mimic |
| `mimic: connexion vers l'amont non établie dans le délai (outbound.connect_timeout)` | HTTP 504; `kind="connect_timeout"` | connection not established in time: NetworkPolicy that drops packets, slow dependency | the call fails with 504 after the timeout | fix the network path; raise `outbound.connect_timeout` if the dependency is legitimately slow |
| `mimic: aucune destination pour le trafic entrant` | HTTP 502 | inbound connection with no original destination and no `inbound.upstream`: a direct call on 15106 | the call fails | call the application on its own port, not 15106 |
| `mimic: requête sans destination (ni proxy explicite, ni redirection)` | HTTP 400 | request sent directly to 15101 without an absolute URI | the call fails | call the real host, or set the proxy variables |
| `mimic: URI invalide` | HTTP 400 | the rebuilt URI cannot be parsed | the call fails | check the request line and the `Host` header |
| `mimic: sonde {} : connexion à {} : {}` | probe exit 1, event `Startup probe failed`, `Readiness probe failed` or `Liveness probe failed` | the admin port does not answer: sidecar still starting, or `admin.listen` changed | pod not ready; a repeated liveness failure restarts the sidecar and the traffic of the pod is cut meanwhile; alerts `MimicSidecarNotReady`, `MimicSidecarRestarting` | wait for startup; check `admin.listen`; `kubectl -n <ns> logs <pod> -c mimic` |
| `mimic: sonde {} : requête vers {} : {}` | probe exit 1 | the admin port accepted the connection but the request failed: overloaded sidecar | pod not ready | look at CPU throttling and `MimicSidecarMemoryHigh` |
| `mimic: sonde {} : réponse de {} : {}` | probe exit 1 | the answer could not be read | pod not ready | same |
| `mimic: sonde {} : {} : statut {}` | probe exit 1; the status is `<code>` or `absent` | non-2xx answer: 503 while the start-up barrier is closed (section 2), or no status line | pod out of service; with a startup probe at 503 the application is held at `Init` | `curl localhost:15100/readyz` after a port-forward (`waiting`, `aligned`, `last-known`, `fallback`); fix the controller link |
