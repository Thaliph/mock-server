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
| webhook | `GET /metrics` sur le port `webhook` (8443), **HTTPS** | certificat `mimic-injector-tls` (CA `mimic-ca`) ; pas de NetworkPolicy livrée |
| contrôleur | `GET /metrics` sur `ui_listen`, **`127.0.0.1:8081`** par défaut, HTTP | port-forward seulement : **non collectable** avec les manifests livrés |

Le port 8443 du contrôleur (API des sidecars) **n'a pas** de `/metrics`. Pour collecter le
contrôleur, il faut changer `ui_listen` en `0.0.0.0:8081`, déclarer le port dans le
Deployment et ouvrir la NetworkPolicy `mimic-controller` au scraper ; mais ce port sert
aussi l'interface et l'API de lecture, **sans authentification** (échanges redactés, OpenAPI,
requêtes enregistrées). À ne faire qu'avec une NetworkPolicy qui n'admet que le scraper. À
défaut, la santé du contrôleur se suit par les métriques des sidecars et kube-state-metrics
(voir [Alertes et supervision](Alertes-et-supervision.md)).

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

## Contrôleur

Exposées par chaque réplique. Les jauges d'état (`workloads`, `pods`, `exchanges`, `exemplars*`)
sont calculées à chaque lecture de `/metrics` par la réplique active ; elles restent à 0 sur la
passive, qui ne touche pas au support.

| Métrique | Type | Labels | Signification |
|----------|------|--------|---------------|
| `mimic_controller_leader` | jauge | | 1 sur la réplique active, 0 sur la passive |
| `mimic_controller_takeover_seconds` | jauge | | durée de la dernière prise de relais : rechargement de l'état persisté jusqu'à la pose du label |
| `mimic_controller_segments_total` | compteur | `result` (`ok`, `duplicate`, `error`) | segments reçus ; `duplicate` : renvoi d'un segment déjà rangé (reprise après erreur réseau, normal) ; `error` : segment illisible ou identifiant invalide (`400`), ou support en échec (`503`, log `ingestion d'un segment`) ; les refus d'authentification n'y sont pas comptés |
| `mimic_controller_auth_failures_total` | compteur | `reason` (`missing`, `invalid`, `forbidden`, `unavailable`) | appels de sidecars refusés : jeton absent, jeton invalide, jeton d'un autre pod ou workload, TokenReview ou lecture du pod impossible (`503`) |
| `mimic_controller_workloads` | jauge | | workloads connus |
| `mimic_controller_pods` | jauge | `state` (`aligned`, `diverging`, `absent`) | pods vus depuis moins de `pod_forget` ; les pods non sélectionnés par un `selector` comptent `aligned` |
| `mimic_controller_snapshots_total` | compteur | | snapshots construits et écrits |
| `mimic_controller_exchanges` | jauge | | échanges en mémoire, tous workloads (borne `max_exchanges_total`) |
| `mimic_controller_exemplars` | jauge | | exemples par endpoint gardés, tous workloads |
| `mimic_controller_exemplars_bytes` | jauge | | taille sérialisée des exemples (borne `max_exemplar_bytes_total`) |
| `mimic_controller_exemplars_refused_total` | compteur | `reason` (`count`, `bytes`) | nouvel endpoint sans exemple (`count`), ou exemple gardé sans body (`bytes`) ; les deux séries existent dès le départ, à 0 |

## Webhook

| Métrique | Type | Labels | Signification |
|----------|------|--------|---------------|
| `mimic_injector_admissions_total` | compteur | `result` (`injected`, `skipped`, `error`) | revues d'admission : pod modifié, laissé tel quel (avec un avertissement `mimic non injecté : …`), ou revue invalide |

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

Écart entre latence servie en mock et latence injectée (coût propre du sidecar) :

```promql
histogram_quantile(0.99, sum by (le, authority) (rate(mimic_request_duration_seconds_bucket{direction="outbound",mode="mock"}[5m])))
- histogram_quantile(0.99, sum by (le, authority) (rate(mimic_mock_injected_latency_seconds_bucket[5m])))
```

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
