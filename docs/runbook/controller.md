# Controller runbook

[Back to the guide](../guide/README.md) · French troubleshooting for the other components:
[Dépannage](../guide/Depannage.md)

Every message the `mimic-controller` shows an operator, with its cause in a real cluster, its
effect on the application and its fix. Messages are in French, **verbatim** as the code emits
them; `<…>` marks a substituted value. Cause and fix are given for the usual case; the
inventory with source lines is `docs/revue/inventaire-erreurs.md` section 1.

A test (`DOC-11`, `crates/mimic-controller/tests/runbook_doc.rs`) fails when a message of the
controller code has no row here with a cause, an effect and a fix.

## How to read the page

- **Level**: `exit 1` / `exit 2` is a message on stderr followed by process exit (the
  container goes to `CrashLoopBackOff`); `WARN`, `ERROR`, `INFO`, `DEBUG` is a JSON log line,
  the text being in `fields.message`; `HTTP NNN` is the body of a response, seen by the sidecar
  in `controller.last_error` as `<METHOD> <path>: <code> <reason> <body>`.
- **Effect on the application**: the controller is never on the data path. Whatever happens
  here, the application keeps being served by its sidecar. What is at stake is the control
  plane: mode switches, snapshots, persistence, discovery. The effect column says which.
- Exit code: configuration errors (`ControllerConfig::load`) exit with code **2**, errors raised
  while starting (`start()`) exit with code **1**.

First commands:

```sh
kubectl -n mimic-system get pods -L mimic.platform.io/leader
kubectl -n mimic-system logs deploy/mimic-controller --all-containers --prefix | grep -v '"INFO"'
kubectl -n mimic-system logs <pod> --previous          # the last words before a crash
kubectl -n shop get mw <workload> -o yaml              # status.conditions
```

## 1. Startup and configuration

The controller reads the ConfigMap `mimic-controller` (file given by `--config` or
`MIMIC_CONTROLLER_CONFIG`). A wrong value never leaves a half-started controller: the process
exits, the rollout stops on the new ReplicaSet and the previous pods keep serving.

| Message | Level | Cause | Effect on the application | Fix |
|---|---|---|---|---|
| `lecture de <fichier>: <err io>` | exit 2 | ConfigMap `mimic-controller` not mounted, wrong key or wrong `subPath` | none for running pods; a new controller never starts, so no mode switch or snapshot | check the volume and the key: `kubectl -n mimic-system get cm mimic-controller -o yaml`, `kubectl -n mimic-system describe pod <pod>` |
| `configuration <fichier> invalide: <err serde_yaml>` | exit 2 | typo in the ConfigMap (unknown field, negative or non-numeric duration, unknown `auth.kind`) | same: the controller does not start | fix the YAML, then `kubectl -n mimic-system rollout restart deploy/mimic-controller` |
| `aggregated exige tls (l'API server joint le contrôleur en HTTPS)` | exit 2 | `aggregated` set without `tls` (partial overlay) | controller does not start; no namespace-scoped read API | add `tls` (cert and key of the Secret `mimic-controller-tls`) |
| `store non supporté: <url>` | exit 2 | `store` without a `file://` or `s3://` prefix, typically a bare path | controller does not start | write `store: file:///var/lib/mimic` (or `s3://<bucket>`) |
| `max_exchanges_per_workload et max_exchanges_total doivent être positifs` | exit 2 | one of the two values is 0 | controller does not start | set a value greater than 0 |
| `max_exemplars_per_workload doit être positif` | exit 2 | value is 0 | controller does not start | set a value greater than 0 |
| `max_exemplar_bytes_per_workload et max_exemplar_bytes_total doivent être positifs` | exit 2 | one of the two values is 0 | controller does not start | set a value greater than 0 |
| `snapshot.max_per_key doit être positif` | exit 2 | value is 0 | controller does not start | set a value greater than 0 |
| `leader_election : lease et namespace requis` | exit 2 | `lease` or `namespace` empty in `leader_election` | controller does not start | fill both (defaults: Lease `mimic-controller` in `mimic-system`) |
| `auth kubernetes exige tls : les sidecars n'acceptent le contrôleur qu'en HTTPS` | exit 2 | `auth: kubernetes` (the default) without `tls`; also what a missing configuration produces | controller does not start; sidecars keep their state and show `délai dépassé` | add the `tls` section; check the ConfigMap is mounted |
| `client Kubernetes: <err>` | exit 1 | `automountServiceAccountToken: false`, no ServiceAccount token, `KUBERNETES_SERVICE_HOST` unset (out of cluster without kubeconfig) | controller does not start | restore the token mount of the ServiceAccount `mimic-controller` |
| `aggregated exige auth kubernetes` | exit 1 | `aggregated` with `auth: static` (a test configuration pushed to a cluster) | controller does not start | use `auth: kubernetes` |
| `leader_election exige auth kubernetes (bail Kubernetes)` | exit 1 (not 2: it is raised by `start()`) | election with `auth: static` (test configuration) | controller does not start | use `auth: kubernetes` |
| `leader_election : identité absente (POD_NAME)` | exit 1 | no `identity`, no `POD_NAME`, no `HOSTNAME`: the Deployment lost the downward-API variable | controller does not start | restore `env: POD_NAME` from `metadata.name` in the Deployment |
| `liste des namespaces (garde-fou production):` | exit 1 when election is off, WARN otherwise (see `prise de relais`) | ClusterRole lacks `list`/`watch` on `namespaces`, or the API server is unreachable | controller does not start; no production guard can be evaluated | apply the shipped RBAC (`deploy/controller/`); `kubectl auth can-i list namespaces --as=system:serviceaccount:mimic-system:mimic-controller` |
| `écoute sur <adresse>: <err>` | exit 1 | port already taken, or `ui_listen` on an IP that is not the pod's | controller does not start | change `listen`, `ui_listen` or `aggregated.listen` |
| `répertoire <dir>: <err>` | exit 1 | `file://` store cannot be created: PVC not mounted, missing `fsGroup` 1338, read-only root | controller does not start; nothing is persisted | check the PVC and `fsGroup`; see [storage](../guide/Exploiter-en-production.md#stockage-du-contrôleur) |
| `bucket <bucket>: <err>` | exit 1 | S3 client cannot be built: region or endpoint missing, malformed URL | controller does not start | fill the `AWS_*` variables of the Secret `mimic-controller-s3`, or the IRSA role |
| `signal SIGTERM: <err>` | exit 1 | signal handler cannot be installed (practically impossible) | controller does not start | none possible in the pod; open an issue with `kubectl logs --previous` |

## 2. Sidecar API and ingestion

What the sidecars call on port 8443. The sidecar logs the same text in `controller.last_error`
and `share.last_error` (`/api/status`), and retries at the next heartbeat: the application is
not affected, the mode and the snapshot simply stop following the controller.

| Message | Level | Cause | Effect on the application | Fix |
|---|---|---|---|---|
| `jeton absent ou invalide` | HTTP 401; `mimic_controller_auth_failures_total{reason="missing"}` or `"invalid"` | no Bearer token, token not authenticated by TokenReview, audience other than `mimic-controller`, user outside `system:serviceaccount:`, pod gone | the pod is not driven: it stays in its configured mode and publishes nothing | check the projected volume `mimic-token` of the pod and `token_audience` of the webhook |
| `jeton valide pour un autre pod ou workload` | HTTP 403; `reason="forbidden"` | `MIMIC_WORKLOAD`, frozen at admission, differs from the current annotation or label; pod without a workload name; `POD_NAME` altered | same as above | make `mimic.platform.io/workload` consistent, recreate the pod |
| `authentification impossible` | WARN, with a 503 to the sidecar | TokenReview or `get pod` failed: ClusterRole lacks `tokenreviews create` or `pods get`, or the API server is down | no pod can authenticate; every sidecar is blind to the controller, applications unaffected | restore the RBAC; check the API server |
| `authentification indisponible` | HTTP 503; `reason="unavailable"` | the HTTP side of `authentification impossible` | same | same |
| `réplique passive du contrôleur : la réplique active sert les sidecars` | HTTP 503 with `Retry-After: 2` | request reached the passive replica (Service endpoint stale, or no `mimic.platform.io/leader` label yet) | transient during a failover, sidecars retry | wait a few seconds; otherwise see `marque de la réplique active` |
| `nom invalide` | HTTP 400 | namespace, workload, instance or file name outside the allowed charset | that pod's calls are refused | use plain DNS-style names for `POD_NAME` and `MIMIC_WORKLOAD` |
| `identifiant de segment invalide:` | HTTP 400 (`synchronisation du partage échouée` on the sidecar) | malformed `segment=` parameter, sidecar of an incompatible version | that pod's captures are not shared | align the sidecar and controller versions |
| `instance réservée:` | HTTP 400 | instance name starting with `_` (forged pod name) | request refused | none; investigate who calls the API |
| `ligne <n>: <err serde>` | HTTP 400 on segment upload | a JSONL line cannot be decoded as an exchange: sidecar newer or older than the controller, or corruption. The sidecar keeps the segment at the head of its queue and retries, so **publication of that pod is blocked** | the pod keeps serving; its captures stop reaching snapshots and discovery | upgrade the controller (or the sidecar) to matching versions; if the segment is corrupt, recreate the pod (the store cannot be emptied under a controller) |
| `ingestion d'un segment` | WARN, with a 503 to the sidecar | storage error during ingestion (index, exemplars, salt, put): volume full or read-only, S3 rights | captures of that pod are not persisted; the sidecar retries | free space, check PVC or S3 rights (see section 3) |
| `support indisponible` | HTTP 503 on segments (`mimic_controller_segments_total{result="error"}`), on the heartbeat, and on the UI | storage unreachable or full (S3, NFS, PVC) | on a heartbeat the sidecar keeps its current state; segments are retried | restore the storage; read the preceding WARN (`ingestion d'un segment`, `état souhaité illisible sur le support`) for the file |
| `battement invalide:` | HTTP 400 | heartbeat JSON unreadable or a required field is missing: incompatible version | the pod follows no mode change | align versions |
| `length limit exceeded` | HTTP 413 (axum default text) | segment above 64 MiB or heartbeat above 64 KiB, typically an enormous `unmatched` list from an old sidecar | that upload is refused | upgrade the sidecar |
| `Failed to deserialize query string:` | HTTP 400 (axum default text) | `instance` missing on a segment POST: a client that is not a mimic sidecar | request refused | none |
| `segment inconnu` | HTTP 404 | segment purged by retention | the requester drops it | none |
| `snapshot inconnu` | HTTP 404 on `/snapshots/<id>` and the UI pin; the sidecar sees `snapshot <id>: GET /snapshots/<id>: 404 Not Found snapshot inconnu` after a silent fallback from `/mock` | the snapshot is missing from the storage: purged by hand, storage restored without it, or corrupted (see `forme de mock impossible`) | the pod stays on its local data; the workload shows `SnapshotNotFound` | `mimicctl snapshot ls`, re-import it or change `spec.snapshot` |
| `forme de mock illisible` | HTTP 500 | decompression of the cached mock form failed (client without `Accept-Encoding`); rare, memory pressure | one download fails, the sidecar retries | retry |
| `forme de mock impossible` | WARN; `/snapshots/<id>/mock` then answers 404 and the sidecar falls back to the full snapshot | the light mock form cannot be built, e.g. `contenu du snapshot <id> altéré (empreinte <x>)`: snapshot file modified or corrupted on the storage | pods download more bytes, mock still served | delete or re-import the snapshot, change `spec.snapshot` |
| `arrêt d'urgence` | WARN (`namespace`, `stop`) | label `mimic.platform.io/emergency-stop` set or removed, by `mimicctl emergency-stop` | at the next heartbeat the pods go to `passthrough` (set) or return to their mode (removed) | intended; find who ran it; undo with `mimicctl emergency-stop --off` |

## 3. Storage

The store (`file://` PVC or `s3://`) holds `desired.json` (the wanted mode), the salt, the
segments, the exemplars and the snapshots. The controller keeps working from memory when it
fails, but loses persistence: a restart then comes back with less. A volume that fills up is the
most common root cause: check the `MimicControllerStorageFull` alert and the free space of the
volume (`kubectl -n mimic-system exec <pod> -- df -h`, with the mount path of your store).
Old snapshots are purged automatically beyond `snapshot.keep_last` at each maintenance, never
the referenced or pinned ones; a volume still full means the purge cannot write (see below)
or the limits are too high.

| Message | Level | Cause | Effect on the application | Fix |
|---|---|---|---|---|
| `état souhaité illisible sur le support` | WARN (`namespace`, `workload`, `error`), 503 `support indisponible` to the heartbeat or the UI | read of `desired.json` fails (other than not found): S3 or NFS unreachable | the sidecar keeps its state; `mimicctl status` and `mode` fail for that workload | check the volume and credentials (`kubectl -n mimic-system get pvc`, `describe pod`), then re-run the failing command |
| `desired.json illisible : état souhaité vide` | ERROR | `desired.json` exists but is invalid JSON: corrupted file, or written by an incompatible version | pods fall back to the mode of their own configuration; a `mock` can be lost | re-apply the `MimicWorkload` (`mimicctl mode …`) to rewrite the file |
| `lecture de l'état souhaité, retentée au prochain appel` | WARN | storage error while reading `desired.json` | stale view until the next call | check the volume and credentials (`kubectl -n mimic-system get pvc`, `describe pod`), then re-run the failing command |
| `lecture de <path>: <err>` | startup error or WARN, depending on the caller | a file of the store cannot be read (the config file at startup, the salt, an exemplar file, a segment) | the feature that needs it is degraded | restore the storage; the `<path>` tells which file |
| `écriture de <path>: <err>` | WARN, or a 503 on ingestion | a file of the store cannot be written: volume full, read-only, S3 rights | data of that file is not persisted | free space, fix the rights |
| `{} vide` (`<path> vide`, e.g. `<ns>/<wl>/redact-salt vide`) | WARN (`sel du workload illisible sur le support`) | salt file empty (truncated write, volume full at the time) | the heartbeat is answered without `redact_salt`; sidecars refuse to capture until a salt exists | delete the empty file so a new salt is drawn: `kubectl -n mimic-system exec deploy/mimic-controller -- rm <data-dir>/<ns>/<wl>/redact-salt` (PVC) or `aws s3 rm s3://<bucket>/<ns>/<wl>/redact-salt` (S3); the next heartbeat redraws it |
| `sel du workload illisible sur le support` | WARN; the heartbeat is answered **without** `redact_salt` | read or write of `<ns>/<wl>/redact-salt` fails, or the file is empty | sidecars without a salt refuse to capture (`no_salt` in their metrics): discovery stops filling | restore the storage; remove an empty file |
| `tirage du sel: <err>` | WARN | the system random generator failed when drawing a new salt | same as above | none usual; recreate the pod, report it |
| `ressource réappliquée à la prochaine maintenance` | WARN | storage error while handling a `MimicWorkload` event; the spec is kept and re-applied within a minute | the mode follows with a delay | none; check the mode followed |
| `liste de <prefix>: <err>` | WARN, via `index des segments` | listing of a workload prefix failed | partial index | check the volume and credentials (`kubectl -n mimic-system get pvc`, `describe pod`), then re-run the failing command |
| `index des segments` | WARN | listing of the segments failed (takeover, maintenance) | discovery and snapshots built from an incomplete list | check the volume and credentials (`kubectl -n mimic-system get pvc`, `describe pod`), then re-run the failing command |
| `relecture des exemples` | WARN | read of `exemplars.jsonl` failed or the file is corrupt | rare endpoints are no longer mockable | restore the storage; if corrupt, record again |
| `relecture des échanges` | WARN | segments cannot be read at the first sight of a workload | discovery and snapshot are poorer | check the volume and credentials (`kubectl -n mimic-system get pvc`, `describe pod`), then re-run the failing command |
| `liste des workloads du support` | WARN | listing of the root of the store failed | persisted workloads are not reloaded | check the volume and credentials (`kubectl -n mimic-system get pvc`, `describe pod`), then re-run the failing command |
| `construction du snapshot échouée` | WARN; condition reason `SnapshotFailed` | build or write of the snapshot failed; details below | the workload stays in its previous snapshot; `Aligned` is false | free space, check rights |
| `construction du snapshot:` | detail of the above | the snapshot cannot be built from the exchanges | same | same |
| `écriture du snapshot <id>: <err>` | detail of the above | snapshot file cannot be written | same | same |
| `métadonnées du snapshot <id>: <err>` | detail of the above | snapshot metadata cannot be written or read | same | same |
| `écriture de l'état souhaité échouée` | WARN | put of `desired.json` failed | the mode applies now, but is **lost at the next controller restart** | restore the storage, then re-apply the mode |
| `écriture des exemples : aucun segment supprimé` | WARN | exemplars flush failed during retention; no segment is deleted until they are written | volume keeps growing | free space, check rights, watch the volume |
| `écriture des exemples : segments gardés` | WARN | same, in the volume-bound branch | same | same |
| `écriture des exemples` | WARN | flush failed at shutdown or maintenance | exemplars of the period may be lost | same |
| `relecture avant suppression` | WARN (`segment`) | a segment to purge could not be read back | segment kept | same |
| `suppression d'un segment` | WARN (`segment`) | delete refused: S3 `DeleteObject` right missing, or read-only filesystem | the volume grows without bound | grant the right; clean by hand |
| `compaction : lecture` | WARN | storage error while reading during compaction | segments stay raw | free space, check rights |
| `compaction : écriture` | WARN | storage error while writing during compaction | same | same |
| `compaction : exemples non écrits, segments bruts gardés` | WARN | exemplars could not be written, raw segments kept | volume grows | same |

## 4. Leader election and high availability

Two replicas, one active (label `mimic.platform.io/leader=true`, which the Service selects). The
passive one refuses sidecar calls with a 503. A failover keeps sidecars serving the mock; only
control is paused for a few seconds (`mimic_controller_takeover_seconds`).

| Message | Level | Cause | Effect on the application | Fix |
|---|---|---|---|---|
| `élection : il faut 0 < retry_period (<n> ms) < renew_deadline (<n> ms) < lease_duration (<n> ms)` | exit 2 (from `mimic-core`) | durations of `leader_election` inconsistent, e.g. `renew_deadline` not below `lease_duration` | controller does not start | respect the order (defaults 2 s < 10 s < 15 s) |
| `lecture du bail` | WARN (passive replica) | `get lease` failed: no `leases get` right, API server issue | a passive replica cannot take over | Role `mimic-controller` in `mimic-system` (`leases`), API server health |
| `prise du bail` | DEBUG (invisible at `info`) | create or patch of the Lease refused (a 409 is normal under contention) | if no replica becomes active, the sidecars see `délai dépassé` | `MIMIC_LOG=debug` on the controller; check `leases create/patch` |
| `écriture du bail` | detail of `prise du bail` | the Lease write error | same | same |
| `conflit : version <v>, attendue <v>` | error of the in-memory lease | only the in-memory lease used by tests returns it; unreachable in a cluster | none | none |
| `bail non renouvelé à temps : la réplique cesse de servir` | ERROR, then exit 1 | no renewal succeeded before `renew_deadline`: slow API server, network, client throttling | the replica exits and restarts (a few seconds); the other one takes over; sidecars ride through | isolated: normal. Recurring: API server latency, `kubectl -n mimic-system get lease mimic-controller -o yaml` |
| `bail tenu par une autre réplique : la réplique cesse de servir` | ERROR, then exit 1 | the Lease changed holder: partition, clock skew, double deployment | same | same, plus check there is one Deployment and synchronized clocks |
| `renouvellement du bail` | WARN | one renewal failed (retried) | none if the next succeeds | if `bail non renouvelé à temps` follows: API server |
| `prise de relais` | WARN, retried every `retry_period` | `list namespaces` failed during takeover (production guard): RBAC or API server. The replica does not serve meanwhile | no active controller; sidecars see `délai dépassé`, keep their mode | restore the RBAC `namespaces list/watch`; `mimic_controller_takeover_seconds` |
| `liste initiale des MimicWorkload non reçue : l'état persisté sert` | WARN after 30 s | the watcher did not deliver the initial list: CRD missing, RBAC, slow API server | the persisted state is served instead of the live one | as for `surveillance des MimicWorkload` |
| `marque de la réplique active` | WARN in a loop | `list` or `patch` of the pods of `mimic-system` refused (see `pods marqués:`, `marque du pod`) | the replica runs but the Service has **no endpoint**: sidecars see `délai dépassé` and nothing is driven. Its `mimic_controller_leader` stays at 0 until the mark is placed (retried every `retry_period`), so `MimicControllerNoLeader` fires after 1 min | add `pods list/patch` to the Role `mimic-controller`; check `kubectl -n mimic-system get endpoints mimic-controller` |
| `pods marqués:` | detail of the above | listing of the labelled pods failed | same | same |
| `marque du pod <pod>: <err>` | detail of the above | label patch on the pod failed | same | same |
| `marque de la réplique active non retirée` | WARN at shutdown | pod patch failed while stopping: API server unreachable | failover waits for the lease to expire (15 s) | none |
| `bail non rendu : il expirera` | WARN at shutdown | Lease write failed while stopping | same | none |

Pod stuck in `ContainerCreating` with `Multi-Attach error`: a `ReadWriteOnce` PVC across two
nodes; apply the `controller-rwo` (or `controller-s3`) variant.

## 5. Aggregated API and UI

The UI (port 8081, by port-forward) and the aggregated read API (`data.mimic.platform.io`, port
8444) are used by `mimicctl` and by developers. Failures never touch the data path.

| Message | Level | Cause | Effect on the application | Fix |
|---|---|---|---|---|
| `aggregated.client_ca_file <chemin>: <err>` | exit 1 | the front-proxy CA file is unreadable | controller does not start | mount the file or remove `client_ca_file` |
| `aggregated sans client_ca_file exige auth kubernetes` | exit 1 | `aggregated` without a CA and with `auth: static` | controller does not start | use `auth: kubernetes` or give `client_ca_file` |
| `kube-system/extension-apiserver-authentication:` | exit 1 | read of the front-proxy ConfigMap refused: RoleBinding `extension-apiserver-authentication-reader` in `kube-system` missing | controller does not start | apply the shipped RBAC |
| `requestheader-client-ca-file absent : l'API server n'a pas de front proxy` | exit 1 | key absent from the ConfigMap: API server without the aggregation layer (some distributions, custom kind) | controller does not start | enable the aggregation layer or set `aggregated.client_ca_file` |
| `requestheader-allowed-names illisible:` | exit 1 | value is not JSON: ConfigMap edited by hand | controller does not start | set `aggregated.allowed_names` explicitly |
| `surveillance des namespaces` | WARN, repeated with backoff | the Namespace watcher errors: `namespaces watch` right missing, API server | the production guard and emergency stop may use stale labels | RBAC of the ClusterRole `mimic-controller` |
| `surveillance des MimicWorkload` | WARN | the CRD watcher errors: CRD missing or too old, RBAC | `MimicWorkload` changes are not seen | `kubectl apply -f deploy/controller/crd.yaml`; RBAC |
| `statut non publié` | WARN (`namespace`, `name`), retried every 5 s | `patch` of the status refused: `mimicworkloads/status` right missing, or a CRD without the status subresource | mode switching works but `kubectl get mw` shows a stale status | RBAC; reapply the CRD |
| `route inconnue` | HTTP 404 (Status `NotFound`), body `<chemin> : route inconnue` | a path outside the API: `mimicctl` or APIService version mismatch | the request fails | align `mimicctl` and controller versions |
| `API de lecture seule : GET uniquement` | HTTP 405 | any verb other than GET on the aggregated API | refused | use GET |
| `X-Remote-User absent` | HTTP 401 | request without the front-proxy headers: direct call or misconfigured front proxy | refused | go through the API server (`kubectl get --raw /apis/data.mimic.platform.io/…`) |
| `demander un RoleBinding du ClusterRole mimic-data-viewer` | HTTP 403, from `<user> ne peut pas <verbe> workloads/<sub> <ns>/<wl> (data.mimic.platform.io) : …` (also `… workloads dans <ns>`, `… workloads sur le cluster`) | the SubjectAccessReview was denied: no RoleBinding | `mimicctl export/requests/report/snapshot ls` fail for that user | bind `mimic-data-viewer` in the workload namespace |
| `SubjectAccessReview impossible` | WARN | the SubjectAccessReview call failed: ClusterRole lacks `subjectaccessreviews create`, API server issue | all reads refused with 503 | RBAC; API server |
| `autorisation impossible :` | HTTP 503 (`ServiceUnavailable`) | the HTTP side of `SubjectAccessReview impossible` | same | same |
| `dépendance inconnue` | HTTP 404 | `export --outbound <authority>` unknown, often without `:port` | none | `mimicctl report` lists the authorities (`host:port`) |
| `direction: inbound ou outbound` | HTTP 400 | invalid `direction` parameter | none | fix the parameter |
| `format: json, jsonl ou k6` | HTTP 400 | invalid `format` parameter | none | fix the parameter |
| `mimic : mock et mock-partial sont refusés dans un namespace labellisé env=production` | admission refusal (ValidatingAdmissionPolicy, Kubernetes 1.30 or later) at `kubectl apply` or `mimicctl mode` | `spec.mode` is `mock` in an `env=production` namespace | none: the mode is not applied | intended; remove the mock |

## 6. Certificates

The controller serves 8443 (sidecars), 8081 (UI) and 8444 (aggregated API) with the Secret
`mimic-controller-tls`, re-read every `tls.reload_interval` seconds (default 10). These
messages come from `mimic-net`, shared with the webhook.

| Message | Level | Cause | Effect on the application | Fix |
|---|---|---|---|---|
| `reload_interval doit être positif` | exit 2 (inside `configuration … invalide`) | `tls.reload_interval: 0` | controller does not start | set a value greater than 0 (default 10) |
| `certificat <fichier>: <err>`, `clé <fichier>: <err>`, `certificat <fichier>: aucun certificat`, `certificat <c> et clé <k>: <err>` | exit 1 | unreadable or empty PEM, or a key that does not match the certificate; usually cert-manager has not issued `mimic-controller-tls` yet | controller does not start | `kubectl -n mimic-system get certificate`, wait for `Ready`, check the mount |
| `CA cliente: <err>` and `CA cliente: aucun certificat` | exit 1 | empty or invalid front-proxy CA | controller does not start | see `kube-system/extension-apiserver-authentication:` |
| `nouveau certificat illisible : l'ancien reste servi` | WARN, once per refused content | hot reload failed: Secret half written, key that does not match | the old certificate keeps being served until it expires, then every sidecar call fails with `invalid peer certificate` | repair the Certificate; `kubectl -n mimic-system describe certificate mimic-controller-tls` |
| `certificat rechargé` | INFO | a renewal was picked up | none | marker only |
| `accept échoué, écoute suspendue 50 ms` | WARN, rate-limited (`omitted`) | `accept()` error, typically EMFILE: too many connections or descriptors | new connections wait 50 ms | raise the descriptor limit, check `max_connections` |
| `arrêt : connexions coupées après le délai de drainage` | WARN at shutdown (`remaining`) | connections still open after draining: long downloads, keep-alive sidecars | those connections are cut and retried | none |
| `poignée de main TLS refusée`, `poignée de main TLS trop longue` | DEBUG | TLS handshake failed or too slow: sidecar with an old CA, scanner, plain-HTTP client | that client cannot reach the controller | set `MIMIC_LOG=debug` to confirm a failed CA rotation; see [CA rotation](../guide/Exploiter-en-production.md#certificats) |

On the sidecar side an expired or mismatched certificate shows as `invalid peer certificate:
BadSignature` (CA reissued without reissuing the controller certificate) or an expiry message.
The metric `mimic_controller_certificate_expiry_timestamp_seconds` and the alert on it warn
before expiry.

## Status conditions of `MimicWorkload`

These are not log messages but what the operator reads in `kubectl get mw -o yaml`.

| Reason | Condition | Cause | Effect on the application | Fix |
|---|---|---|---|---|
| `InvalidMode`, `InvalidCapture`, `InvalidMock` | `Aligned=False` | `spec.mode`, `spec.capture.inbound` or `spec.mock.websocket` unknown (old CRD) | the previous state keeps applying | fix the value, update the CRD |
| `SnapshotNotFound` | `Aligned=False` | `spec.snapshot` absent from the storage | the previous state keeps applying | `mimicctl snapshot ls` or `import`, pin it |
| `SnapshotFailed` | `Aligned=False` | see `construction du snapshot échouée` | same | free space, check rights |
| `NoPods` | `Aligned=False` | no selected pod seen within `pod_timeout`: pods not injected, selector too strict, heartbeats failing | nothing to drive | check injection and the UI |
| `PodsDiverging` | `Aligned=False`; `mimic_controller_pods{state="diverging"}` | a pod does not report the wanted mode or snapshot: failed download, sidecar too old | that pod serves something else | logs of the diverging pod |
| `ProductionNamespace` | `Allowed=False` | mock requested in an `env=production` namespace, state goes back to `record` | no mock | intended |
| `EmergencyStop` | `Allowed=False` | emergency-stop label on the namespace or on `mimic-system`; pods in `passthrough` | no capture, no mock | `mimicctl emergency-stop --off` |
| `Empty` | `SnapshotEmpty=True` | mock with a snapshot holding no replayable outbound exchange: built before any outbound traffic, or capture `off` | every outbound call answers `599` | record traffic, switch back to `record` then `mock` |

### Messages carried by the conditions

Some conditions carry a French `message`, shown by `kubectl -n <ns> get mw <workload> -o jsonpath='{.status.conditions}'`.

| Message | Condition | Cause | Effect on the application | Fix |
|---|---|---|---|---|
| `arrêt d'urgence : label mimic.platform.io/emergency-stop=true sur le namespace ou sur mimic-system, pods en passthrough` | `Allowed=False`, reason `EmergencyStop` | someone labelled the namespace or `mimic-system` with `mimic.platform.io/emergency-stop=true` (`mimicctl emergency-stop`) | pods are forced to `passthrough`: no capture, no mock; traffic is proxied untouched | find the label (`kubectl get ns -l mimic.platform.io/emergency-stop=true`), then `mimicctl emergency-stop --off` once the incident is closed |
| `mock et mock-partial sont refusés dans un namespace labellisé env=production` | `Allowed=False`, reason `ProductionNamespace` | `spec.mode` is `mock` or `mock-partial` in a namespace labelled `env=production` | the mock is not applied; the state goes back to `record` and the real dependencies are called | intended; set `spec.mode: record` (`mimicctl mode record`), or use a non-production namespace |
| `le snapshot servi ne contient aucun échange sortant rejouable` | `SnapshotEmpty=True`, reason `Empty` | mock with a snapshot built before any outbound traffic was recorded, or capture was `off` | every outbound call answers `599`: the application sees failing dependencies | `mimicctl mode record`, let outbound traffic flow, build a new snapshot (`mimicctl snapshot ls` to check its exchanges), then `mimicctl mode mock` |
