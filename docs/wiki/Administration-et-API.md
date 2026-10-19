mimic a trois surfaces d'administration : la page et l'API de chaque sidecar, l'interface en
lecture seule du contrôleur, et l'API du contrôleur réservée aux sidecars. Sources :
`crates/mimic-proxy/src/admin.rs`, `crates/mimic-controller/src/{ui,api,auth,leader}.rs`.

## Vue d'ensemble des accès

| Surface | Écoute | Protocole | Authentification | Qui y accède |
|---------|--------|-----------|------------------|--------------|
| admin du sidecar | `0.0.0.0:15100` (port `mimic-admin`) | HTTP | **aucune** | sondes du kubelet, scraper, opérateurs par port-forward ; **tout pod du cluster** qui peut joindre l'IP du pod |
| interface du contrôleur | `127.0.0.1:8081` | HTTP | aucune ; l'accès passe par `kubectl port-forward` (RBAC `pods/portforward` `create` dans `mimic-system`) | opérateurs, `mimicctl export/requests/report` |
| API du contrôleur | `0.0.0.0:8443`, Service `mimic-controller:8443` | HTTPS | jeton de ServiceAccount projeté, validé par TokenReview (audience `mimic-controller`) | sidecars uniquement (NetworkPolicy : pods labellisés `mimic.platform.io/sidecar=true`) |

> **Écart avec la spec.** La spec (§9.2) place l'admin du sidecar sur localhost, joignable par
> port-forward. L'injecteur la configure sur `0.0.0.0:15100`, ce qu'exigent les sondes HTTP du
> kubelet et la collecte des métriques. `PUT /mode` et `DELETE /store` ne sont pas
> authentifiés : sur un sidecar **sans** contrôleur, n'importe quel pod qui joint le port
> peut changer son mode ou purger son store. Sous contrôleur, les deux sont refusés (`403`).
> Restreindre le port 15100 par NetworkPolicy au scraper si besoin (les sondes du kubelet ne
> sont pas concernées par les NetworkPolicy sur la plupart des CNI).

## Admin du sidecar

```sh
kubectl -n shop port-forward pod/checkout-7d9f8c6b5-abcde 15100
open http://127.0.0.1:15100/
```

Sous contrôleur, la page ne montre que **ce pod** : son store ne contient que ce qu'il a vu.
Le workload entier est dans l'interface du contrôleur.

| Méthode, route | Réponse | Erreurs |
|----------------|---------|---------|
| `GET /healthz`, `GET /readyz` | `ok` | |
| `GET /` | page HTML : mode, instance ; sous contrôleur, `N échanges vus par ce pod`, `piloté par le contrôleur · snapshot <id>`, état de la publication et dernière erreur ; sans contrôleur, échanges locaux et importés, pairs ; catalogue des opérations ; liens OpenAPI et métriques | |
| `GET /api/operations` | catalogue JSON des opérations découvertes | |
| `GET /api/status` | JSON : `mode`, `capture_inbound`, `mock_websocket`, `instance`, `workload` (`namespace`, `name`), `store` (`local`, `peer`), `share` (`enabled`, `peers`, `last_sync_ms`, `last_error`), `snapshot`, `controller` (`url`, `desired`, `last_ok_ms`, `last_error`, ou `null` sans contrôleur) | |
| `GET /openapi/inbound` | OpenAPI de l'API du service, vue par ce pod | |
| `GET /openapi/outbound` | liste des authorities sortantes découvertes | |
| `GET /openapi/outbound/{authority}` | OpenAPI d'une dépendance (`api.stripe.com:443`) | `404 dépendance inconnue` |
| `GET /mode` | mode courant, en texte | |
| `PUT /mode` | body `mock` ou `{"mode":"mock"}` (1 Ko au plus) ; renvoie le nouveau mode | `400 mode inconnu (passthrough, record, mock, mock-partial)`, `413 body trop gros`, `403` sous contrôleur |
| `GET /metrics` | métriques OpenMetrics | |
| `GET /exchanges?direction=inbound\|outbound&limit=n` | derniers échanges en mémoire, redactés (100 par défaut, 10 000 au plus) | `400 direction: inbound ou outbound` |
| `DELETE /store` | `store purgé` : mémoire et segments du journal local | `403` sous contrôleur |
| `POST /share/sync` | `202 synchronisation demandée` : publication immédiate (utile avant un `kubectl delete pod`) | `404 partage désactivé` |
| autre | `404 introuvable` | |

Refus sous contrôleur : `403 sidecar piloté par le contrôleur : basculer via la ressource MimicWorkload`.

Exemples :

```sh
curl -s http://127.0.0.1:15100/api/status | jq '{mode, snapshot, controller}'
curl -s 'http://127.0.0.1:15100/exchanges?direction=outbound&limit=5' | jq '.[].path'
curl -s http://127.0.0.1:15100/openapi/outbound | jq .
curl -X POST http://127.0.0.1:15100/share/sync
```

> **Écart avec la spec.** Le filtre `GET /exchanges?path=…` de la spec (§9.2) n'existe pas ;
> `GET /api/status` et `GET /openapi/outbound` (liste) existent mais ne sont pas dans la spec.

## Interface du contrôleur

```sh
kubectl port-forward -n mimic-system \
  "$(kubectl -n mimic-system get pod -l mimic.platform.io/leader=true -o name)" 8081
open http://127.0.0.1:8081/
```

Viser la **réplique active** (label `mimic.platform.io/leader=true`) : `deploy/mimic-controller`
peut désigner la passive, qui répond `503 réplique passive du contrôleur : la réplique active sert les sidecars`
à tout sauf `/healthz` et `/metrics`.

### Pages

| Route | Contenu |
|-------|---------|
| `/` | « Workloads observés » : namespace, workload, mode souhaité (ou `libre` sans `MimicWorkload`), pods alignés, échanges en mémoire ; lien vers `/metrics` |
| `/ui/{ns}/{wl}` | mode souhaité, snapshot, alignement et raison ; commandes `kubectl patch` prêtes à copier (et la commande `kubectl apply` si la ressource n'existe pas) ; pods avec leur dernier battement et leur état (`aligné`, `en cours d'alignement`, `absent`, `non sélectionné`) ; API découvertes (service et dépendances) ; snapshots ; 50 dernières requêtes et liens d'export k6, JSONL, JSON |

### API de lecture

Toutes en `GET`, JSON sauf mention contraire. `{ns}` et `{wl}` doivent être des noms valides
(`400 nom invalide` sinon).

| Route | Réponse |
|-------|---------|
| `/healthz` | `ok` |
| `/metrics` | métriques du contrôleur (voir [Métriques](Metriques#contrôleur)) |
| `/v1/workloads` | pour chaque workload : `namespace`, `name`, `spec`, `desired`, `pods` (`instance`, `mode`, `snapshot`, `capture_inbound`, `mock_websocket`, `last_seen_ms`, `state`, `selected`), `selected_pods`, `aligned`, `ready`, `reason` |
| `/v1/workloads/{ns}/{wl}/mode` | état souhaité courant (`mode`, `snapshot`, `capture_inbound`, `mock_websocket`, `selector`) |
| `/v1/workloads/{ns}/{wl}/snapshots` | snapshots sur le support : `id`, `size`, `modified_ms`, du plus récent au plus ancien |
| `/v1/workloads/{ns}/{wl}/snapshots/{id}` | snapshot complet, JSONL (`404 snapshot inconnu`) |
| `/v1/workloads/{ns}/{wl}/operations` | catalogue des opérations du workload entier |
| `/v1/workloads/{ns}/{wl}/openapi/inbound` | OpenAPI du service |
| `/v1/workloads/{ns}/{wl}/openapi/outbound/{authority}` | OpenAPI d'une dépendance (`404 dépendance inconnue`) |
| `/v1/workloads/{ns}/{wl}/requests?direction=&limit=&format=` | échanges enregistrés : `direction` `inbound` (défaut) ou `outbound`, `limit` 1000 par défaut (100 000 au plus), `format` `json` (avec `instance`), `jsonl` ou `k6` (script de rejeu). Erreurs : `400 direction: inbound ou outbound`, `400 format: json, jsonl ou k6` |

```sh
curl -s http://127.0.0.1:8081/v1/workloads | jq '.[] | {namespace, name, aligned, reason}'
curl -s http://127.0.0.1:8081/v1/workloads/shop/checkout/snapshots | jq '.[0]'
curl -s 'http://127.0.0.1:8081/v1/workloads/shop/checkout/requests?format=k6&limit=2000' > replay.js
```

Les données servies sont déjà redactées, mais restent des échanges réels (chemins, paramètres
non sensibles, bodies non JSON). Traiter l'accès `pods/portforward` sur `mimic-system` comme
un accès à ces données.

## API du contrôleur pour les sidecars

Réservée aux sidecars ; à connaître pour le diagnostic. Chaque appel porte
`Authorization: Bearer <jeton projeté>`. Le contrôleur valide le jeton par TokenReview
(audience `mimic-controller`, résultat gardé 60 s), lit le pod correspondant et vérifie que
le namespace, le workload (annotation `mimic.platform.io/workload` ou labels) et l'instance
(nom du pod) de l'URL sont **les siens**.

| Méthode, route | Usage | Réponses |
|----------------|-------|----------|
| `GET /healthz` | sondes | `ok` |
| `POST /v1/workloads/{ns}/{wl}/heartbeat` | battement : `{instance, mode, snapshot, capture_inbound, mock_websocket}` → état souhaité du pod | `200` |
| `POST /v1/workloads/{ns}/{wl}/segments?instance=&segment=` | segment JSONL (64 MiB au plus) | `201` nouveau, `200` doublon ; `400 identifiant de segment invalide: …`, `400 instance réservée: …`, `400 ligne <n>: …` ; `503 support indisponible` |
| `GET /v1/workloads/{ns}/{wl}/segments`, `GET …/segments/{instance}/{file}` | lecture des segments | `404 segment inconnu` |
| `GET /v1/workloads/{ns}/{wl}/snapshots/{id}` | snapshot complet (sidecars d'une version antérieure) | `404 snapshot inconnu` |
| `GET /v1/workloads/{ns}/{wl}/snapshots/{id}/mock` | forme de mock : `ETag`, `Cache-Control: private, max-age=31536000, immutable`, `Vary: accept-encoding`, `zstd`, `gzip` ou identité ; `304` sur `If-None-Match` | `404 snapshot inconnu` |

| Refus | Body | `reason` de `mimic_controller_auth_failures_total` |
|-------|------|------------------------------------------------------|
| `401` | `jeton absent ou invalide` | `missing`, `invalid` |
| `403` | `jeton valide pour un autre pod ou workload` | `forbidden` (y compris un pod sans nom de workload) |
| `503` | `authentification indisponible` | `unavailable` (TokenReview ou lecture du pod en échec) |

Un `503` avec `Retry-After: 2` et le body de la réplique passive signale que l'appel a atteint
la passive.

## Droits Kubernetes

| Rôle livré (`deploy/controller/rbac.yaml`) | Portée | Droits |
|--------------------------------------------|--------|--------|
| `mimic-controller` (ClusterRole) | contrôleur | `mimicworkloads` `get`, `list`, `watch` ; `mimicworkloads/status` `patch` ; `tokenreviews` `create` ; `pods` `get` ; `namespaces` `get`, `list`, `watch` |
| `mimic-controller` (Role dans `mimic-system`) | contrôleur | `leases` `create` ; `leases/mimic-controller` `get`, `patch` ; `pods` `list`, `patch` (label de la réplique active) |
| `mimic-operator` (ClusterRole, à lier par namespace) | équipes qui basculent | tous les verbes sur `mimicworkloads` ; `namespaces` `get` (garde-fou de `mimicctl`) |
| `mimic-viewer` (ClusterRole, agrégé à `view`) | lecture | `mimicworkloads` en lecture |

Pour `mimicctl export`, `requests` et `report` (et l'interface) : `pods` `list` et
`pods/portforward` `create` dans `mimic-system`, à accorder séparément.

```sh
kubectl -n shop create rolebinding perf-mimic-operator \
  --clusterrole=mimic-operator --group=perf-team
```
