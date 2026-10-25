# Métriques

[← Sommaire](README.md)

Toutes les métriques exposées par mimic, d'après `crates/mimic-proxy/src/metrics.rs`,
`crates/mimic-controller/src/metrics.rs` et `crates/mimic-injector/src/server.rs`. Format
OpenMetrics (`prometheus_client`) : un compteur déclaré `mimic_requests` est exposé
`mimic_requests_total` ; un histogramme donne `_bucket`, `_sum` et `_count`.

## Où les collecter

| Composant | Endpoint | Accès |
|-----------|----------|-------|
| sidecar | `GET /metrics` sur le port `mimic-admin` (15100) de chaque pod injecté, HTTP | depuis le réseau du cluster (écoute `0.0.0.0`) |
| webhook | `GET /metrics` sur le port `webhook` (8443), **HTTPS**, sans jeton | certificat `mimic-injector-tls` (CA `mimic-ca`) ; aucune NetworkPolicy livrée pour le webhook : tout le cluster peut le joindre |
| contrôleur | `GET /metrics` sur le port `api` (8443) de chaque réplique, **HTTPS**, sans jeton | depuis un namespace labellisé `mimic.platform.io/monitoring: "true"` : la NetworkPolicy `mimic-controller` lui ouvre 8443 |

Le port 8443 du contrôleur est celui de l'API des sidecars : `/metrics` et `/healthz` y
répondent **sans jeton**, sur les deux répliques — l'active comme la passive, qui donne
`mimic_controller_leader 0` et des jauges d'état à 0. Il n'expose ni l'interface ni l'API de lecture, qui vivent sur
`ui_listen` (`127.0.0.1:8081` par défaut, port-forward seulement) : collecter le contrôleur
ne publie donc aucun échange enregistré. Le certificat est celui de `mimic-controller-tls`
(CA `mimic-ca`) : le scraper doit l'approuver, ou ignorer la vérification.

`deploy/example/vmpodscrape-controller.yaml` livre la collecte du contrôleur **et** celle du
webhook (opérateur VictoriaMetrics) :

```yaml
apiVersion: operator.victoriametrics.com/v1beta1
kind: VMPodScrape
metadata: {name: mimic-controller, namespace: mimic-system}
spec:
  selector:
    matchLabels: {app.kubernetes.io/name: mimic-controller}
  podMetricsEndpoints:
    - port: api
      path: /metrics
      scheme: https
      tlsConfig:
        ca:
          secret: {name: mimic-controller-ca, key: ca.crt}
        # Le pod est joint par son IP : le nom vérifié est celui du certificat.
        serverName: mimic-controller.mimic-system.svc
```

Équivalent Prometheus Operator : le même contenu en `PodMonitor`
(`monitoring.coreos.com/v1`), avec `tlsConfig.ca.secret` et `tlsConfig.serverName`. Le
namespace du collecteur doit porter le label qu'attend la NetworkPolicy, et `vmagent` lire
les `VMPodScrape` de `mimic-system` (`selectAllByDefault`, ou son sélecteur) :

```sh
kubectl label namespace monitoring mimic.platform.io/monitoring=true
```

Exemple de collecte des sidecars (opérateur VictoriaMetrics, `deploy/example/vmpodscrape.yaml`) :

```yaml
apiVersion: operator.victoriametrics.com/v1beta1
kind: VMPodScrape
metadata: {name: mimic}
spec:
  selector:
    matchExpressions:
      - {key: app.kubernetes.io/name, operator: Exists}
  podMetricsEndpoints:
    - {port: mimic-admin, path: /metrics}
```

Équivalent Prometheus Operator : un `PodMonitor` avec `podMetricsEndpoints: [{port: mimic-admin, path: /metrics}]`
et un sélecteur sur `mimic.platform.io/sidecar: "true"` (label posé par le webhook).

Les labels `namespace` et `pod` viennent du scraper. Les métriques du sidecar portent en plus
un label constant **`version`** quand il est connu : `MIMIC_VERSION` (label
`app.kubernetes.io/version` du pod, réglable par `version_label` du webhook), sinon
`version` de la configuration, sinon `MIMIC_POD_TEMPLATE_HASH` ; tronqué à 63 caractères.

## Sidecar

Label `authority` : `hôte:port` de la destination (sortant) ou du header `Host` (entrant).
Au-delà de **100** authorities distinctes par processus, les suivantes sont regroupées sous
`other` (cardinalité bornée).

### Trafic

| Métrique | Type | Labels | Signification |
|----------|------|--------|---------------|
| `mimic_requests_total` | compteur | `direction` (`inbound`, `outbound`), `mode`, `authority`, `method`, `status` | requêtes HTTP traitées, y compris les réponses produites par mimic (`599`, `502`, `503`, `403`, `413`) et les `CONNECT` |
| `mimic_request_duration_seconds` | histogramme | `direction`, `mode`, `authority` | durée vue par l'appelant ; en `passthrough`, jusqu'au premier octet de la réponse ; sinon jusqu'au dernier ; en mock, latence servie. Seaux : 0,5 ms × 2ⁿ, 16 seaux de 0,5 ms à ~16,4 s, puis `+Inf` |
| `mimic_upstream_errors_total` | compteur | `direction`, `authority`, `kind` (`connect`, `request`) | échecs vers l'amont (dépendance en sortie, application en entrée), dans tous les modes qui relaient ; chaque échec donne un `502` |
| `mimic_passthrough_connections_total` | compteur | `direction`, `kind` (`tls`, `tcp`), `result` (`relayed`, `refused`, `error`) | connexions redirigées non HTTP : `relayed`, tunnel ouvert ; `refused`, refermée en `mock` sans joindre la destination : TLS non intercepté, ou TCP non HTTP et connexion restée muette avec `mock.tcp: refuse` (le défaut) ; `error`, destination injoignable. Chaque refus donne aussi un log `warn` (`… refusée en mode mock`, champs `destination` et `kind`), au plus une ligne par seconde avec `omitted` |
| `mimic_websocket_connections_total` | compteur | `direction`, `result` (`relayed`, `refused`) | poignées de main WebSocket : `101` relayé, ou refus en mock |
| `mimic_tls_handshake_failures_total` | compteur | `authority`, `reason` (`client_rejected`) | poignées de main TLS interceptées refusées par l'application (CA mimic non approuvée, épinglage) |
| `mimic_connections_refused_total` | compteur | `listener` (`inbound`, `outbound`, `admin`) | connexions acceptées puis refermées sans être lues, au plafond `server.max_connections` (2 048 par défaut, commun à l'entrant et au sortant, tunnels et WebSocket compris ; l'admin a son propre plafond fixe de 64). Chaque refus donne un log `warn` (`plafond de connexions simultanées atteint, connexion refusée`), au plus une ligne par seconde avec `omitted` |

### Mock

| Métrique | Type | Labels | Signification |
|----------|------|--------|---------------|
| `mimic_mode` | jauge | `mode` (les quatre) | 1 pour le mode courant, 0 pour les autres |
| `mimic_mock_responses_total` | compteur | `authority`, `match` (`exact`, `template`, `fuzzy`, `unmatched`, `too_large`) | décisions du moteur ; `unmatched` compte aussi les `CONNECT` refusés en `mock` et les requêtes relayées en `mock-partial` faute d'enregistrement ; `too_large` : body de requête au-delà de `capture.max_body_bytes`, refusé en `413` avant toute recherche. Chaque `599` et chaque `413` donne aussi un log `warn` (`mock : requête sans réponse enregistrée`, `mock : body de requête trop gros` ; champs `method`, `authority`, `path` en forme sans valeur, `trace_id`, `reason`), au plus une ligne par seconde et par raison avec `omitted`, et une entrée dans `GET /api/unmatched` ([Administration](Administration-et-API.md#requêtes-sans-réponse-du-mock)) |
| `mimic_mock_injected_latency_seconds` | histogramme | `authority` | latence tirée et appliquée aux réponses mockées (mêmes seaux) |
| `mimic_mock_snapshot_exchanges` | jauge | | échanges du snapshot chargé ; 0 sans snapshot |
| `mimic_mock_engine_builds_total` | compteur | `trigger` (`switch`, `refresh`, `unmatched`) | constructions du moteur : bascule de mode ou de snapshot, reconstruction en tâche de fond (sans snapshot, au plus 1/s), requête sans correspondance dans un moteur périmé |

### Capture, store et partage

| Métrique | Type | Labels | Signification |
|----------|------|--------|---------------|
| `mimic_store_exchanges` | jauge | `origin` (`local`, `peer`) | échanges en mémoire, calculé à la lecture de `/metrics` ; `peer` seulement en partage direct |
| `mimic_store_bytes` | jauge | | octets estimés du store, à comparer à `capture.max_store_bytes` |
| `mimic_store_disk_bytes` | jauge | | octets des fichiers sous `store.dir` (journal `local-<n>.jsonl`, état souhaité, formes de mock gardées), calculé à la lecture de `/metrics` ; 0 sans `store.dir` |
| `mimic_store_disk_limit_bytes` | jauge | | taille de l'`emptyDir` du store, posée par le webhook (`MIMIC_STORE_SIZE_LIMIT_BYTES`, 512 Mi) ; 0 si inconnue. Au-delà, le kubelet évince le pod |
| `mimic_capture_dropped_total` | compteur | `reason` | voir ci-dessous |
| `mimic_share_sync_total` | compteur | `result` (`ok`, `error`) | cycles de publication (et d'import en partage direct) ; un cycle sans rien à publier compte `ok` sans appeler le contrôleur |
| `mimic_share_peers` | jauge | | autres instances vues au dernier partage ; toujours 0 sous contrôleur |
| `mimic_discovered_operations` | jauge | `direction`, `authority` (`inbound.upstream` ou `app` pour l'entrant) | opérations découvertes ; **mise à jour seulement** quand la découverte est calculée (page d'admin, `/api/operations`, `/openapi/*`) : peut être périmée |

### Lien avec le contrôleur

Exposées par les sidecars pilotés (`share.url` vers le contrôleur) seulement.

| Métrique | Type | Labels | Signification |
|----------|------|--------|---------------|
| `mimic_heartbeats_total` | compteur | `result` (`ok`, `error`) | battements toutes les `share.mode_poll` (2 s) ; `ok` : le contrôleur a répondu un état souhaité (même si son snapshot n'a pas pu être téléchargé : voir `last_error` dans `/api/status`) ; `error` : réseau, délai, code autre que `200`, corps illisible |
| `mimic_heartbeat_last_success_timestamp_seconds` | jauge | | date Unix du dernier battement `ok` ; 0 avant le premier |
| `mimic_startup` | jauge | `state` (`waiting`, `aligned`, `last-known`, `fallback`) | 1 pour l'état courant de la barrière de démarrage ([Basculer en mock](Basculer-en-mock.md#pods-neufs-pendant-un-mock)) : `waiting`, application pas encore démarrée ; `aligned`, état souhaité appliqué ; `last-known`, délai écoulé, dernier état connu appliqué ; `fallback`, délai écoulé sans état connu, repli `share.startup.fallback` appliqué |

| `reason` de `mimic_capture_dropped_total` | Signification | Gravité |
|-------------------------------------------|---------------|---------|
| `truncated` | body au-delà de `capture.max_body_bytes`, gardé en préfixe (non rejouable) | informatif ; monter la limite si l'opération doit être mockée |
| `sample_limit` | body non gardé : plafond `samples_per_operation` atteint pour (opération, code) | normal, croît avec le trafic |
| `store_budget` | body retiré ou échange évincé de la mémoire (`max_store_bytes`, bodies au plus pour moitié) | normal une fois le store plein (fenêtre glissante) |
| `local_backlog` | ligne non écrite dans le journal local (file d'écriture pleine) | disque lent ou plein |
| `share_backlog` | échange abandonné avant publication (`share.max_backlog_bytes` ou `max_pending`) | **perte de données** pour le contrôleur |
| `unredactable` | body que la redaction ne sait pas lire (XML, texte, gRPC, binaire, sans type), réduit à sa taille faute d'exception `redact.raw_bodies` | normal ; la réponse n'est pas rejouable en mock |
| `no_salt` | échange abandonné : aucun sel connu (sidecar démarré par repli, contrôleur pas encore joint) | transitoire ; durable, voir le lien avec le contrôleur |

### Version

| Métrique | Type | Labels | Signification |
|----------|------|--------|---------------|
| `mimic_build_info` | jauge | `mimic_version`, `revision` | toujours 1 : version de mimic et commit de l'image (`unknown` hors image). Exposée aussi par le contrôleur et le webhook. Le label s'appelle `mimic_version` parce que `version` est déjà celui de l'application sur les séries du sidecar |

## Contrôleur

Exposées par chaque réplique. Les jauges d'état (`workloads`, `pods`, `exchanges`, `exemplars*`)
sont calculées à chaque lecture de `/metrics` par la réplique active ; elles restent à 0 sur la
passive, qui ne touche pas au support.

| Métrique | Type | Labels | Signification |
|----------|------|--------|---------------|
| `mimic_controller_leader` | jauge | | 1 sur la réplique active une fois la marque posée (le label `mimic.platform.io/leader` que sélectionne le Service), 0 sinon : sur la passive, et sur l'active tant que la marque n'est pas posée ; ce n'est pas « bail tenu » |
| `mimic_controller_takeover_seconds` | jauge | | durée de la dernière prise de relais : rechargement de l'état persisté jusqu'à la pose du label |
| `mimic_controller_segments_total` | compteur | `result` (`ok`, `duplicate`, `error`) | segments reçus ; `duplicate` : renvoi d'un segment déjà rangé (reprise après erreur réseau, normal) ; `error` : segment illisible ou identifiant invalide (`400`), ou support en échec (`503`, log `ingestion d'un segment`) ; les refus d'authentification n'y sont pas comptés |
| `mimic_controller_auth_failures_total` | compteur | `reason` (`missing`, `invalid`, `forbidden`, `unavailable`) | appels de sidecars refusés : jeton absent, jeton invalide, jeton d'un autre pod ou workload, TokenReview ou lecture du pod impossible (`503`) |
| `mimic_controller_workloads` | jauge | | workloads connus |
| `mimic_controller_pods` | jauge | `workload` (`<namespace>/<nom>`), `state` (`aligned`, `diverging`, `absent`) | pods de chaque workload vus depuis moins de `pod_forget` ; les pods non sélectionnés par un `selector` comptent `aligned`. Les trois états de chaque workload connu existent, à 0 s'il le faut. Le namespace du workload est dans `workload`, pas dans `namespace`, qui est celui du pod du contrôleur |
| `mimic_controller_snapshots_total` | compteur | | snapshots construits et écrits |
| `mimic_controller_snapshots_deleted_total` | compteur | | snapshots purgés (cycle de vie, `snapshot.keep_last`) : hors référencés (`spec.snapshot`, `status.snapshot`) et épinglés (`mimicctl snapshot pin`) |
| `mimic_controller_exchanges` | jauge | | échanges en mémoire, tous workloads (borne `max_exchanges_total`) |
| `mimic_controller_exemplars` | jauge | | exemples par endpoint gardés, tous workloads |
| `mimic_controller_exemplars_bytes` | jauge | | taille sérialisée des exemples (borne `max_exemplar_bytes_total`) |
| `mimic_controller_exemplars_refused_total` | compteur | `reason` (`count`, `bytes`) | nouvel endpoint sans exemple (`count`), ou exemple gardé sans body (`bytes`) ; les deux séries existent dès le départ, à 0 |
| `mimic_controller_certificate_expiry_timestamp_seconds` | jauge | | date Unix d'expiration du certificat servi sur le port `api`, suivie à chaque rechargement à chaud ; 0 sans TLS. Exposée aussi par la passive |

## Webhook

| Métrique | Type | Labels | Signification |
|----------|------|--------|---------------|
| `mimic_injector_admissions_total` | compteur | `result` (`injected`, `skipped`, `error`) | revues d'admission : pod modifié, laissé tel quel (avec un avertissement `mimic non injecté : …`), ou revue invalide |
| `mimic_injector_certificate_expiry_timestamp_seconds` | jauge | | date Unix d'expiration du certificat servi à l'API server ; 0 sans TLS |

## Requêtes PromQL utiles

Débit et taux d'erreur de l'API du service (entrant), par pod :

```promql
sum by (namespace, pod) (rate(mimic_requests_total{direction="inbound"}[5m]))

sum by (namespace, pod) (rate(mimic_requests_total{direction="inbound", status=~"5.."}[5m]))
  / sum by (namespace, pod) (rate(mimic_requests_total{direction="inbound"}[5m]))
```

Latence p99 par dépendance, à comparer entre `record` et `mock` :

```promql
histogram_quantile(0.99,
  sum by (le, authority, mode) (rate(mimic_request_duration_seconds_bucket{direction="outbound"}[5m])))
```

Écart entre latence servie en mock et latence injectée (coût propre du sidecar). Les deux
histogrammes partagent les mêmes seaux, de 0,5 ms à ~16 s : une **différence de quantiles**
n'y mesure rien, car le surcoût du sidecar (quelques dizaines de microsecondes) est bien en
dessous du premier seau et les deux p99 tombent dans le même. Il faut comparer les
**moyennes**, qui sont exactes (`_sum` / `_count`) :

```promql
  sum by (namespace, pod) (rate(mimic_request_duration_seconds_sum{direction="outbound",mode="mock"}[5m]))
/ sum by (namespace, pod) (rate(mimic_request_duration_seconds_count{direction="outbound",mode="mock"}[5m]))
-
  sum by (namespace, pod) (rate(mimic_mock_injected_latency_seconds_sum[5m]))
/ sum by (namespace, pod) (rate(mimic_mock_injected_latency_seconds_count[5m]))
```

À lire seulement quand le mock répond à tout : `mimic_mock_injected_latency_seconds` ne
compte que les réponses servies depuis un enregistrement, alors que
`mimic_request_duration_seconds{mode="mock"}` compte aussi les `599`, les `413` et les
`CONNECT` refusés. Vérifier d'abord que `mimic_mock_responses_total{match="unmatched"}` reste
à zéro (requête ci-dessous).

Part de requêtes sans correspondance pendant un tir, par dépendance :

```promql
sum by (namespace, authority) (rate(mimic_mock_responses_total{match="unmatched"}[5m]))
  / sum by (namespace, authority) (rate(mimic_mock_responses_total[5m]))
```

Qualité des correspondances (`template` et `fuzzy` élevés : body ou query diffèrent des
enregistrements) :

```promql
sum by (match) (rate(mimic_mock_responses_total{match!="unmatched"}[5m]))
```

Pods en mock sans snapshot (servent leurs seules données locales) :

```promql
(mimic_mode{mode="mock"} == 1) and on (namespace, pod) (mimic_mock_snapshot_exchanges == 0)
```

Mode de chaque pod :

```promql
max by (namespace, pod, mode) (mimic_mode == 1)
```

Pods qui n'ont pas eu de réponse du contrôleur depuis plus d'une minute, et pods démarrés
sans état souhaité (repli) ou dont l'application attend encore :

```promql
time() - mimic_heartbeat_last_success_timestamp_seconds > 60
max by (namespace, pod, state) (mimic_startup{state=~"fallback|waiting"} == 1)
```

Échec de publication vers le contrôleur (pods qui ont quelque chose à publier) :

```promql
sum by (namespace, pod) (rate(mimic_share_sync_total{result="error"}[10m]))
  / sum by (namespace, pod) (rate(mimic_share_sync_total[10m]))
```

Pertes de données et remplissage du store :

```promql
sum by (namespace, pod) (increase(mimic_capture_dropped_total{reason="share_backlog"}[15m]))
max by (namespace, pod) (mimic_store_bytes)
```

Workloads dont des pods n'appliquent pas l'état souhaité (contrôleur) :

```promql
max by (workload) (mimic_controller_pods{state="diverging"}) > 0
```

Pods par version de mimic, après une mise à jour (sidecars pas encore redémarrés) :

```promql
count by (mimic_version) (mimic_build_info)
```

Comparaison de versions (label `version`) :

```promql
histogram_quantile(0.99,
  sum by (le, version) (rate(mimic_request_duration_seconds_bucket{direction="inbound"}[5m])))
```

Ressources du sidecar (cAdvisor ; conteneur `mimic`) :

```promql
sum by (namespace, pod) (rate(container_cpu_usage_seconds_total{container="mimic"}[5m]))
max by (namespace, pod) (container_memory_working_set_bytes{container="mimic"})
```

Coût d'un `599` dans l'application : `mimic_requests_total{direction="outbound",status="599"}`
ne doit pas être confondu avec une erreur de la dépendance ; un `502` avec
`mimic_upstream_errors_total` correspondant est, lui, une vraie dépendance injoignable.
