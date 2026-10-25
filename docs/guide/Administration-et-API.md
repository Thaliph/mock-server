# Administration et API

[← Sommaire](README.md)

mimic a trois surfaces d'administration : la page et l'API de chaque sidecar, l'interface en
lecture seule du contrôleur, et l'API du contrôleur réservée aux sidecars. Sources :
`crates/mimic-proxy/src/admin.rs`, `crates/mimic-controller/src/{ui,api,auth,leader}.rs`.

## Vue d'ensemble des accès

| Surface | Écoute | Protocole | Authentification | Qui y accède |
|---------|--------|-----------|------------------|--------------|
| admin du sidecar | `0.0.0.0:15100` (port `mimic-admin`) | HTTP | aucune ; hors `/healthz`, `/readyz` et `/metrics`, seules les connexions de la boucle locale (`127.0.0.1`, `::1`) sont servies, les autres reçoivent `403` | sondes du kubelet et scraper (`/healthz`, `/readyz`, `/metrics`) ; opérateurs par `kubectl port-forward` ou `kubectl exec` pour le reste |
| interface du contrôleur | `127.0.0.1:8081` | HTTP | aucune ; l'accès passe par `kubectl port-forward` (RBAC `pods/portforward` `create` dans `mimic-system`) | exploitants ; `mimicctl snapshot pin/import/gc`, et les lectures de `mimicctl` sur un cluster sans l'API agrégée |
| API agrégée de lecture | `0.0.0.0:8444` (port `aggregated`), par l'API server : `/apis/data.mimic.platform.io/v1alpha1` | HTTPS | certificat client du front proxy de l'API server ; utilisateur des en-têtes `X-Remote-*`, autorisé par `SubjectAccessReview` | `mimicctl export/requests/report/snapshot ls/snapshot export`, `kubectl get --raw` ; équipes ayant `mimic-data-viewer` dans leur namespace |
| API du contrôleur | `0.0.0.0:8443` (port `api`), Service `mimic-controller:8443` | HTTPS | jeton de ServiceAccount projeté, validé par TokenReview (audience `mimic-controller`) ; **sauf `/healthz` et `/metrics`**, servis sans jeton par les deux répliques | sidecars (NetworkPolicy : pods labellisés `mimic.platform.io/sidecar=true`) ; scraper de métriques (namespace labellisé `mimic.platform.io/monitoring=true`) |

> L'admin écoute sur `0.0.0.0:15100`, ce qu'exigent les sondes HTTP du kubelet et la collecte
> des métriques ; les pages, l'API et les ordres (`PUT /mode`, `DELETE /store`,
> `/share/sync`) ne répondent qu'à la boucle locale (spec §9.2, SEC-07). `127.0.0.6`, d'où
> Istio en mode sidecar relaie les autres pods, est refusée ; le webhook exclut de plus 15100
> de l'interception entrante d'Istio et de Linkerd ([Service mesh](Service-mesh.md#port-admin-hors-du-mesh)),
> pour qu'aucun mesh ne relaie vers l'admin depuis `127.0.0.1`. Le proxy de l'API Kubernetes
> (`kubectl get --raw …/proxy/…`) n'atteint donc que les sondes et `/metrics`.

## Admin du sidecar

```sh
kubectl -n shop port-forward pod/checkout-7d9f8c6b5-abcde 15100
open http://127.0.0.1:15100/
```

Sous contrôleur, la page ne montre que **ce pod** : son store ne contient que ce qu'il a vu.
Le workload entier est dans l'interface du contrôleur.

| Méthode, route | Réponse | Erreurs |
|----------------|---------|---------|
| `GET /healthz` | `ok` : vivacité, ne dépend ni du contrôleur ni du démarrage | |
| `GET /readyz` | sous contrôleur, état de la barrière de démarrage : `aligned`, `last-known` ou `fallback` (`200`) ; sans contrôleur, `ok` | `503` `waiting` : l'application ne doit pas encore démarrer (premier état souhaité ou son snapshot attendu) |
| `GET /` | page HTML : mode, instance ; sous contrôleur, `N échanges vus par ce pod`, `piloté par le contrôleur · snapshot <id>`, état de la publication et dernière erreur ; sans contrôleur, échanges locaux et importés, pairs ; catalogue des opérations ; liens OpenAPI et métriques | |
| `GET /api/operations` | catalogue JSON des opérations découvertes | |
| `GET /api/unmatched?limit=n` | requêtes sans réponse du mock depuis l'entrée en mock (ci-dessous) : `total` et `entries` (100 clés par défaut et au plus) | |
| `GET /api/status` | JSON : `mode`, `capture_inbound`, `mock_websocket`, `instance`, `workload` (`namespace`, `name`), `store` (`local`, `peer`), `share` (`enabled`, `peers`, `last_sync_ms`, `last_error`), `snapshot`, `controller` (`url`, `desired`, `last_ok_ms`, `last_error`, `startup` : `state`, `fallback`, `timeout` en secondes ; ou `null` sans contrôleur) | |
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
curl -s 'http://127.0.0.1:15100/api/unmatched?limit=10' | jq '.entries[] | "\(.method) \(.authority)\(.path) ×\(.count)"'
curl -X POST http://127.0.0.1:15100/share/sync
```

### Requêtes sans réponse du mock

`GET /api/unmatched` répond à la question « pourquoi ce `599` ? » : chaque requête que le mock
refuse (`599` sans enregistrement correspondant avec `mock.unmatched: error`, `413` body au-delà
de `capture.max_body_bytes`) y est agrégée par clé (authority, méthode, forme du chemin,
raison).

```json
{
  "total": 53,
  "entries": [
    {"authority": "api.stripe.com:443", "method": "GET", "path": "/v1/customers/{id}",
     "reason": "unmatched", "count": 52, "first_ms": 1792677780000, "last_ms": 1792677787000},
    {"authority": "quote:8080", "method": "POST", "path": "/upload",
     "reason": "too_large", "count": 1, "first_ms": 1792677781000, "last_ms": 1792677781000}
  ]
}
```

| Champ | Sens |
|-------|------|
| `total` | requêtes refusées depuis l'entrée en mock, y compris celles des clés évincées |
| `path` | **forme** du chemin, jamais ses valeurs : identifiants en `{int}`, `{uuid}`, `{id}` ; e-mail, carte, JWT, IBAN ou marqueur en `{redacted}` ; jeton ou segment long en `{opaque}` ; ni query, ni headers, ni body ; 256 caractères au plus |
| `reason` | `unmatched` (`599`) ou `too_large` (`413`) |
| `count`, `first_ms`, `last_ms` | occurrences, première et dernière (ms Unix) |

Bornes : 100 clés en mémoire ; une clé nouvelle évince la moins récemment vue. Tri : la plus
récemment vue d'abord. Le relevé repart de zéro quand le sidecar entre en `mock` ou
`mock-partial` depuis un autre mode, ou charge un autre snapshot ; il reste lisible après le
retour en `record`. Une requête relayée faute de correspondance (`mock-partial`,
`mock.unmatched: passthrough`) n'y figure pas. Sous contrôleur, les 20 clés les plus récentes
remontent dans le battement : voir `/v1/workloads` et `mimicctl report` pour le workload entier.

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
| `/ui/{ns}/{wl}` | mode souhaité, snapshot, alignement et raison ; commandes `kubectl patch` prêtes à copier (et la commande `kubectl apply` si la ressource n'existe pas) ; pods avec leurs requêtes sans réponse, leur dernier battement et leur état (`aligné`, `en cours d'alignement`, `absent`, `non sélectionné`) ; requêtes sans réponse du mock agrégées sur les pods sélectionnés (méthode, dépendance, forme du chemin, `599` ou `413`, occurrences, dernière) ; API découvertes (service et dépendances) ; snapshots ; 50 dernières requêtes et liens d'export k6, JSONL, JSON |

### API de lecture

Toutes en `GET`, JSON sauf mention contraire. `{ns}` et `{wl}` doivent être des noms valides
(`400 nom invalide` sinon).

| Route | Réponse |
|-------|---------|
| `/healthz` | `ok` |
| `/metrics` | métriques du contrôleur (voir [Métriques](Metriques.md#contrôleur)) |
| `/v1/workloads` | pour chaque workload : `namespace`, `name`, `spec`, `desired`, `pods` (`instance`, `mode`, `snapshot`, `capture_inbound`, `mock_websocket`, `unmatched` tel qu'annoncé, absent pour un sidecar antérieur, `last_seen_ms`, `state`, `selected`), `selected_pods`, `aligned`, `ready`, `reason`, `unmatched` (relevé fusionné des pods sélectionnés : totaux additionnés, occurrences des mêmes clés additionnées, 50 clés au plus, même format que `/api/unmatched` du sidecar) |
| `/v1/workloads/{ns}/{wl}/mode` | état souhaité courant (`mode`, `snapshot`, `capture_inbound`, `mock_websocket`, `selector`) |
| `/v1/workloads/{ns}/{wl}/snapshots` | snapshots sur le support : `id`, `size`, `modified_ms`, du plus récent au plus ancien |
| `/v1/workloads/{ns}/{wl}/snapshots/{id}` | snapshot complet, JSONL (`404 snapshot inconnu`) |
| `/v1/workloads/{ns}/{wl}/operations` | catalogue des opérations du workload entier |
| `/v1/workloads/{ns}/{wl}/openapi/inbound` | OpenAPI du service |
| `/v1/workloads/{ns}/{wl}/openapi/outbound/{authority}` | OpenAPI d'une dépendance (`404 dépendance inconnue`) |
| `/v1/workloads/{ns}/{wl}/requests?direction=&limit=&format=` | échanges enregistrés : `direction` `inbound` (défaut) ou `outbound`, `limit` 1000 par défaut (100 000 au plus), `format` `json` (avec `instance`), `jsonl` ou `k6` (script de rejeu). Erreurs : `400 direction: inbound ou outbound`, `400 format: json, jsonl ou k6` |

```sh
curl -s http://127.0.0.1:8081/v1/workloads | jq '.[] | {namespace, name, aligned, reason}'
curl -s http://127.0.0.1:8081/v1/workloads | jq '.[] | select(.unmatched.total > 0) | {name, unmatched}'
curl -s http://127.0.0.1:8081/v1/workloads/shop/checkout/snapshots | jq '.[0]'
curl -s 'http://127.0.0.1:8081/v1/workloads/shop/checkout/requests?format=k6&limit=2000' > replay.js
```

Les données servies sont déjà redactées, mais restent des échanges réels (chemins, paramètres
non sensibles, bodies non JSON). Traiter l'accès `pods/portforward` sur `mimic-system` comme
un accès à ces données, pour **tous** les namespaces.

## API agrégée de lecture

Les mêmes lectures, namespace par namespace, passent par l'API server (spec §9.2c) :
l'`APIService` `v1alpha1.data.mimic.platform.io` renvoie le groupe vers le port `aggregated`
(8444) de la réplique active. Le contrôleur n'accepte que le certificat client du front proxy
de l'API server (CA et noms lus dans `kube-system/extension-apiserver-authentication` au
démarrage), puis autorise chaque requête par `SubjectAccessReview` (décision gardée 10 s).

| Chemin (préfixe `/apis/data.mimic.platform.io/v1alpha1`) | Droit vérifié | Route de l'interface |
|-----------------------------------------------------------|---------------|----------------------|
| `/workloads` | `list workloads` sur le cluster | `/v1/workloads` |
| `/namespaces/<ns>/workloads` | `list workloads` dans `<ns>` | `/v1/workloads`, réduit à `<ns>` |
| `/namespaces/<ns>/workloads/<wl>/operations` | `get workloads/operations` | `…/operations` |
| `/namespaces/<ns>/workloads/<wl>/openapi/inbound`, `…/openapi/outbound/<authority>` | `get workloads/openapi` | `…/openapi/…` |
| `/namespaces/<ns>/workloads/<wl>/requests?direction=&format=&limit=` | `get workloads/requests` | `…/requests` |
| `/namespaces/<ns>/workloads/<wl>/snapshots`, `…/snapshots/<id>` | `get workloads/snapshots` | `…/snapshots…` |

```sh
kubectl get --raw /apis/data.mimic.platform.io/v1alpha1/namespaces/shop/workloads/checkout/operations | jq
kubectl -n shop create rolebinding shop-mimic-data --clusterrole=mimic-data-viewer --group=shop-devs
```

Refus : `403` avec un `Status` Kubernetes qui nomme l'utilisateur et l'accès ;
`SubjectAccessReview` impossible : `503`. Lecture seule : toute autre méthode que `GET` reçoit
`405`. État de l'APIService : `kubectl get apiservice v1alpha1.data.mimic.platform.io`
(condition `Available`).

## API du contrôleur pour les sidecars

Réservée aux sidecars ; à connaître pour le diagnostic. Chaque appel porte
`Authorization: Bearer <jeton projeté>`, sauf `/healthz` et `/metrics`. Le contrôleur valide le jeton par TokenReview
(audience `mimic-controller`, résultat gardé 60 s), lit le pod correspondant et vérifie que
le namespace, le workload (annotation `mimic.platform.io/workload` ou labels) et l'instance
(nom du pod) de l'URL sont **les siens**.

| Méthode, route | Usage | Réponses |
|----------------|-------|----------|
| `GET /healthz` | sondes du kubelet, sur les deux répliques, sans jeton | `ok` |
| `GET /metrics` | collecte Prometheus, sans jeton, sur les deux répliques (voir [Métriques](Metriques.md#où-les-collecter)) | métriques du contrôleur |
| `POST /v1/workloads/{ns}/{wl}/heartbeat` | battement : `{instance, mode, snapshot, capture_inbound, mock_websocket, unmatched}` → état souhaité du pod. `unmatched` (relevé des 20 clés les plus récentes) est facultatif : un contrôleur d'une version antérieure l'ignore, un relevé illisible est ignoré sans refuser le battement, et le contrôleur n'en garde que 20 clés | `200` ; `413` au-delà de 64 KiB |
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
| `mimic-controller` (ClusterRole) | contrôleur | `mimicworkloads` `get`, `list`, `watch` ; `mimicworkloads/status` `patch` ; `tokenreviews` `create` ; `subjectaccessreviews` `create` ; `pods` `get` ; `namespaces` `get`, `list`, `watch` |
| `extension-apiserver-authentication-reader` (Role standard de `kube-system`, RoleBinding `mimic-controller-auth-reader`) | contrôleur | lecture du ConfigMap de la CA du front proxy |
| `mimic-controller` (Role dans `mimic-system`) | contrôleur | `leases` `create` ; `leases/mimic-controller` `get`, `patch` ; `pods` `list`, `patch` (label de la réplique active) |
| `mimic-operator` (ClusterRole, à lier par namespace) | équipes qui basculent | tous les verbes sur `mimicworkloads` ; `namespaces` `get` (garde-fou de `mimicctl`) |
| `mimic-viewer` (ClusterRole, agrégé à `view`) | lecture | `mimicworkloads` en lecture |
| `mimic-data-viewer` (ClusterRole, à lier par namespace) | équipes qui lisent leurs données | `workloads` et `workloads/{operations,openapi,requests,snapshots}` du groupe `data.mimic.platform.io` en `get`, `list` |
| `mimic-cli` (Role dans `mimic-system`) | exploitants | `pods` `get`, `list` ; `pods/portforward` `create` : interface du contrôleur, tous namespaces |

`mimicctl export`, `requests`, `report` et `snapshot ls|export` n'ont besoin que de
`mimic-data-viewer` dans le namespace du workload ; `snapshot pin|import|gc` et l'interface web,
de `mimic-cli`.

```sh
kubectl -n shop create rolebinding perf-mimic-operator \
  --clusterrole=mimic-operator --group=perf-team
```
