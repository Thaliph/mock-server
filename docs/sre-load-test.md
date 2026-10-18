# Tir de charge d'un workload isolé

Guide SRE pour mesurer la latence et la consommation d'un pod sous charge, sans ses
dépendances, puis pour l'intégrer à un déploiement blue/green. Référence : spec §5.8.

Ce que fait mimic pendant le tir :

- les **dépendances** du workload (appels sortants) sont servies depuis un snapshot des
  réponses enregistrées, avec leur latence enregistrée ; rien ne sort du pod. Chaque pod n'en
  télécharge que la forme de mock (spec §5.6) : les réponses rejouables et les durées par
  opération, compressées, les mêmes octets sur tous les pods (sur le banc d'arrivée d'un pod,
  0,29 Mo au lieu des 47,4 Mo du snapshot complet) ;
- le **trafic entrant** arrive toujours à l'application : c'est elle qu'on mesure ;
- avec `--capture-inbound metadata` (ou `off`), le sidecar ne copie plus les bodies entrants
  et n'en publie rien : son coût reste négligeable devant celui de l'application, et le store
  ne grossit pas avec le trafic du tir ;
- avec `--selector`, seuls les pods choisis basculent (par exemple le nouveau ReplicaSet) ;
- exception : une connexion WebSocket sortante n'a pas de réponse rejouable (les trames ne
  sont pas enregistrées) et reste relayée vers la vraie dépendance. Avec
  `--mock-websocket refuse`, le sidecar la refuse en `503` (`x-mimic-websocket: refused`) et
  plus rien ne sort du pod ; les refus se comptent dans
  `mimic_websocket_connections_total{result="refused"}`.

> **Garde-fou production.** Dans un namespace labellisé `env=production`, `mock` et
> `mock-partial` sont refusés : par `mimicctl` (code de sortie 2), par le contrôleur
> (conditions `Aligned` et `Allowed` à `False`, raison `ProductionNamespace`, les sidecars ne
> basculent jamais) et, si elle est installée, par la politique d'admission
> `deploy/controller/production-guard.yaml`. Les tirs se font donc dans un namespace de perf ou
> de préproduction.

## Prérequis

- Le workload tourne avec mimic injecté (`deploy/README.md`) dans un namespace hors
  production, ici `perf`, en `record` depuis assez longtemps pour avoir vu les appels de ses
  dépendances (`mimicctl report checkout -n perf` les liste). Le contrôleur ne garde que
  24 heures d'historique, mais aussi un exemple de chaque endpoint au-delà (spec §5.5.3) : un
  appel vu une seule fois, même il y a plusieurs jours, est encore rejoué en mock et figure
  dans `mimicctl requests`. Ces exemples sont bornés en octets (64 MiB par workload par
  défaut) : si `mimic_controller_exemplars_refused_total{reason="bytes"}` augmente, certains
  endpoints n'ont qu'un exemple sans body et ne sont plus rejoués au-delà des 24 heures.
- Les métriques des sidecars sont collectées (`deploy/example/vmpodscrape.yaml`), avec les
  labels `namespace` et `pod` posés par le scraper, ainsi que celles du kubelet (cAdvisor :
  `container_cpu_usage_seconds_total`, `container_memory_working_set_bytes`).
- Pour comparer des versions, le pod porte `app.kubernetes.io/version` (label repris par
  l'injecteur, réglable par `version_label`), sinon c'est `pod-template-hash` qui sert de
  version. Toutes les métriques du sidecar portent alors `version="…"`.
- `k6` sur le poste ou dans un Job, et les droits du rôle `mimic-operator` sur le namespace.

## 1. Préparer le rejeu

```sh
mimicctl requests checkout -n perf --format k6 --limit 2000 > replay.js
```

Le script rejoue les requêtes entrantes réellement enregistrées. Les headers redactés
(jetons, cookies) ne sont pas rejoués : fournir un `Authorization` valide par
`-e AUTHORIZATION=…`. Avec `metadata` ou `off`, le trafic du tir n'entre pas dans ce corpus :
le prochain `replay.js` sera toujours tiré du trafic réel.

## 2. Isoler le workload

```sh
mimicctl mode checkout mock --capture-inbound metadata -n perf
kubectl apply -n perf -f deploy/example/networkpolicy-mock.yaml   # facultatif : rien ne sort
# Le workload ouvre des WebSockets vers ses dépendances ? Les refuser aussi, sinon la
# NetworkPolicy les fait échouer en 502 :
#   mimicctl mode checkout mock --capture-inbound metadata --mock-websocket refuse -n perf
until mimicctl status checkout -n perf | tail -1 | grep -qE ' ([0-9]+)/\1$'; do sleep 2; done
```

`ALIGNED` doit être complet (`3/3`) : chaque pod sélectionné applique le mode, le snapshot et
la capture demandés. Noter l'identifiant du snapshot (`mimicctl status`) : le passer à
`--snapshot` lors du tir suivant rejoue exactement les mêmes réponses de dépendances, ce qui
rend deux tirs comparables d'une semaine à l'autre.

Pour ne tirer que sur certains pods, par exemple un pod d'une nouvelle version :

```sh
mimicctl mode checkout mock --selector pod-template-hash=7d9f8c6b5 --capture-inbound metadata -n perf
```

Les pods non sélectionnés reçoivent un état vide : ils gardent le mode de leur configuration
(`record` avec l'injecteur), sans snapshot, et ne comptent pas dans `ALIGNED`.

## 3. Tirer

```sh
k6 run -e TARGET=http://checkout.perf:8080 -e VUS=50 -e ITERATIONS=20000 replay.js
```

Avec `--selector`, viser un Service qui ne sélectionne que ces pods (par exemple le Service de
preview d'un blue/green), sinon une partie du tir arrive aux autres pods.

## 4. Mesurer

Latence vue par l'appelant, par version (les buckets vont de 0,5 ms à 16 s, en doublant : la
précision d'un quantile est celle d'un bucket) :

```promql
histogram_quantile(0.99, sum by (le, version) (
  rate(mimic_request_duration_seconds_bucket{namespace="perf", direction="inbound"}[1m])))
```

Débit et codes :

```promql
sum by (version, status) (rate(mimic_requests_total{namespace="perf", direction="inbound"}[1m]))
```

CPU et mémoire du conteneur de l'application, par version (jointure sur le pod ; toute série
du sidecar convient, `mimic_mode` en a une par mode) :

```promql
sum by (version) (
  rate(container_cpu_usage_seconds_total{namespace="perf", container="checkout"}[1m])
  * on (namespace, pod) group_left (version)
    max by (namespace, pod, version) (mimic_mode))

sum by (version) (
  container_memory_working_set_bytes{namespace="perf", container="checkout"}
  * on (namespace, pod) group_left (version)
    max by (namespace, pod, version) (mimic_mode))
```

CPU par requête, indépendant du débit du tir (c'est la grandeur à suivre pour dimensionner) :

```promql
sum by (version) (
  rate(container_cpu_usage_seconds_total{namespace="perf", container="checkout"}[1m])
  * on (namespace, pod) group_left (version) max by (namespace, pod, version) (mimic_mode))
/
sum by (version) (rate(mimic_requests_total{namespace="perf", direction="inbound"}[1m]))
```

Contrôles du mock pendant le tir :

| Requête | Attendu |
|---------|---------|
| `sum(rate(mimic_mock_responses_total{namespace="perf", match="unmatched"}[1m]))` | 0 : sinon une dépendance répond `599`, le tir mesure des erreurs |
| `histogram_quantile(0.99, sum by (le) (rate(mimic_mock_injected_latency_seconds_bucket{namespace="perf"}[1m])))` | proche de la latence réelle des dépendances |
| `sum(rate(mimic_websocket_connections_total{namespace="perf", direction="outbound"}[1m])) by (result)` | `relayed` : WebSockets relayés vers les vraies dépendances ; `refused` : refusés (`--mock-websocket refuse`) |
| `rate(container_cpu_usage_seconds_total{namespace="perf", container="mimic"}[1m])` | faible devant celui de l'application |

## 5. Revenir en record

```sh
mimicctl mode checkout record -n perf
kubectl delete -n perf -f deploy/example/networkpolicy-mock.yaml --ignore-not-found
```

Un apply sans `--selector` ni `--capture-inbound` les retire : tout le workload revient en
`record` avec la capture de sa configuration.

## Blue/green avec Argo Rollouts (exemple)

Scénario : la version courante (blue) tourne et enregistre ; un nouveau ReplicaSet (green)
apparaît derrière le Service de preview ; avant la promotion, seul green passe en mock, reçoit
le rejeu des appels enregistrés, et ses p99 et CPU par requête sont comparés à des seuils. Si
la performance tient, Argo promeut green, qui sert alors le trafic **sans** mock (le Job remet
le workload en `record`).

Ce n'est qu'un exemple : mimic ne dépend pas d'Argo, tout passe par `mimicctl` et les
métriques Prometheus. À jouer dans un namespace hors production (garde-fou), par exemple un
environnement de préproduction qui déploie avec la même stratégie.

Points d'attention :

- Argo Rollouts pose `rollouts-pod-template-hash` sur les pods, pas `pod-template-hash` :
  c'est ce label que vise `--selector`. Pour que le label `version` des métriques suive le
  ReplicaSet, poser `app.kubernetes.io/version` dans le template du pod, ou régler l'injecteur
  avec `version_label: rollouts-pod-template-hash`.
- Les deux versions ne reçoivent pas la même charge (blue sert le trafic réel, green le tir) :
  comparer des p99 bruts entre elles n'a pas de sens. L'analyse compare green à des seuils,
  par exemple ceux mesurés lors du tir de la version précédente (même snapshot, même
  `replay.js`) ; le CPU par requête, lui, se compare d'une version à l'autre.

```yaml
apiVersion: argoproj.io/v1alpha1
kind: Rollout
metadata:
  name: checkout
  namespace: preprod
spec:
  # … template du pod, avec le label app.kubernetes.io/version
  strategy:
    blueGreen:
      activeService: checkout
      previewService: checkout-preview
      autoPromotionEnabled: true
      prePromotionAnalysis:
        templates:
          - templateName: checkout-load-test
        args:
          - name: preview-hash
            valueFrom:
              podTemplateHashValue: Latest
          - name: version
            value: "1.5.0"   # app.kubernetes.io/version du nouveau template
---
apiVersion: argoproj.io/v1alpha1
kind: AnalysisTemplate
metadata:
  name: checkout-load-test
  namespace: preprod
spec:
  args:
    - name: preview-hash
    - name: version
    - name: p99-max-seconds
      value: "0.250"
    - name: cpu-per-request-max      # cœur·seconde par requête, mesuré au tir précédent + marge
      value: "0.004"
  metrics:
    # 1. Le tir : green seul en mock, rejeu des appels enregistrés, retour en record.
    - name: load-test
      provider:
        job:
          spec:
            backoffLimit: 0
            template:
              spec:
                serviceAccountName: mimic-load-test   # rôle mimic-operator + pods/portforward dans mimic-system
                restartPolicy: Never
                containers:
                  - name: k6
                    image: registry.example.com/platform/mimic-load-test:1.0   # k6 + mimicctl
                    command: ["/bin/sh", "-ec"]
                    args:
                      - |
                        trap 'mimicctl mode checkout record -n preprod' EXIT
                        mimicctl requests checkout -n preprod --format k6 --limit 2000 > /tmp/replay.js
                        mimicctl mode checkout mock -n preprod --capture-inbound metadata \
                          --selector rollouts-pod-template-hash={{args.preview-hash}}
                        until mimicctl status checkout -n preprod | tail -1 | grep -qE ' ([0-9]+)/\1$'; do sleep 2; done
                        # ITERATIONS élevé : le tir dure --duration, pas le temps d'épuiser le corpus.
                        k6 run -e TARGET=http://checkout-preview:8080 -e VUS=50 -e ITERATIONS=100000000 \
                          --duration 5m /tmp/replay.js
    # 2. La latence de green pendant le tir (requête évaluée après le tir).
    - name: p99
      initialDelay: 6m
      count: 1
      successCondition: result[0] <= {{args.p99-max-seconds}}
      provider:
        prometheus:
          address: http://vmselect.monitoring:8481/select/0/prometheus
          query: |
            histogram_quantile(0.99, sum by (le) (
              rate(mimic_request_duration_seconds_bucket{namespace="preprod",
                direction="inbound", mode="mock", version="{{args.version}}"}[5m])))
    # 3. Le CPU par requête de l'application de green, indépendant du débit du tir.
    - name: cpu-per-request
      initialDelay: 6m
      count: 1
      successCondition: result[0] <= {{args.cpu-per-request-max}}
      provider:
        prometheus:
          address: http://vmselect.monitoring:8481/select/0/prometheus
          query: |
            sum(rate(container_cpu_usage_seconds_total{namespace="preprod", container="checkout"}[5m])
                * on (namespace, pod) group_left (version)
                  max by (namespace, pod, version) (mimic_mode{version="{{args.version}}"}))
            /
            sum(rate(mimic_requests_total{namespace="preprod", direction="inbound",
                mode="mock", version="{{args.version}}"}[5m]))
```

Le filtre `mode="mock"` ne garde que les séries de green pendant le tir : blue, resté en
`record`, n'entre pas dans le calcul. Si l'analyse échoue, Argo ne promeut pas green ; dans
tous les cas le `trap` du Job remet le workload en `record`, et green, une fois promu, sert
le trafic sans mock.
