# mimicctl

`mimicctl` pilote mimic depuis un poste ou une CI : bascule des workloads, statut, export
de ce qui a été découvert, rejeu et comparaison d'API. Référence : spec §9.5.

## Installation et accès

```sh
cargo install --path crates/mimicctl
```

`mimicctl` utilise le kubeconfig courant (`KUBECONFIG`, contexte et namespace par défaut
compris) et n'a que les droits de son utilisateur :

| Commandes | Droits Kubernetes nécessaires |
|-----------|-------------------------------|
| `mode` | `patch` et `create` sur `mimicworkloads` du namespace (rôle `mimic-operator`) |
| `status` | `list` sur `mimicworkloads` (rôle `mimic-viewer`, agrégé à `view`) |
| `export`, `requests`, `report` | `list` sur les pods et `create` sur `pods/portforward` dans `mimic-system` |
| `load <workload>` | ceux de `requests`, plus `get`/`list` sur `pods` de `metrics.k8s.io` dans le namespace du workload (relevés CPU et mémoire) ; aucun avec `--from` sans workload |
| `diff` | aucun : travaille sur deux fichiers |

`export`, `requests`, `report` et `load` ouvrent un port-forward vers le pod du contrôleur (port
8081), comme `kubectl port-forward` ; aucun port n'est exposé hors du cluster. Le pod choisi
est la réplique active, marquée `mimic.platform.io/leader=true` ; pendant une bascule, la
passive répond `503` et la commande est à relancer quelques secondes plus tard.

Les options `-n/--namespace` prennent le namespace du contexte par défaut. Codes de sortie :
`0` succès, `1` changement cassant détecté par `diff --fail-on-breaking`, `2` erreur ou refus
(garde-fou production de `mode`, isolement de `load`)
(message sur la sortie d'erreur, préfixé `mimicctl:`).

## Commandes

### `mode` : basculer un workload

```sh
mimicctl mode <workload> <passthrough|record|mock|mock-partial> [--snapshot <id>] \
  [--selector k=v[,k=v…]] [--capture-inbound full|metadata|off] \
  [--mock-websocket relay|refuse] [-n ns]
```

Crée ou met à jour la ressource `MimicWorkload` du workload (server-side apply, gestionnaire
`mimicctl`). Tous les pods du workload basculent ensemble, sans redémarrage. En `mock`, le
contrôleur construit un snapshot des réponses enregistrées et tous les pods servent le
même ; `--snapshot` rejoue un snapshot existant (par exemple celui d'un tir précédent).
Un mode inconnu est refusé avant tout appel au cluster.

**Garde-fou production** : dans un namespace labellisé `env=production`, `mock` et
`mock-partial` sont refusés avec le code de sortie 2, sans rien appliquer. `mimicctl` lit
pour cela les labels du namespace (`get` sur `namespaces`, inclus dans `mimic-operator`) ;
s'il ne le peut pas, il le signale et applique, et c'est le contrôleur qui refuse (conditions
`Aligned` et `Allowed` à `False`, raison `ProductionNamespace`).

```sh
mimicctl mode checkout mock -n shop
mimicctl mode checkout mock --snapshot 3f9a0c1e5b7d2a46 -n shop
mimicctl mode checkout record -n shop
```

Seules les dépendances du workload sont mockées : son trafic entrant continue d'arriver à
l'application. C'est ce qui permet de charger un workload isolé de ses dépendances.

Pour un tir de charge (guide : [sre-load-test.md](sre-load-test.md)) :

- `--selector k=v[,k=v…]` (`spec.selector.matchLabels`) : seuls les pods qui portent ces
  labels basculent, par exemple le ReplicaSet de la nouvelle version
  (`pod-template-hash=7d9f8c6b5`). Les autres reçoivent un état vide et gardent le mode de
  leur configuration (`record` avec l'injecteur). Le contrôleur lit les labels dans l'API,
  pas dans ce qu'annonce le sidecar.
- `--capture-inbound metadata|off` (`spec.capture.inbound`) : pendant le tir, le sidecar ne
  garde de l'entrant que les métadonnées, en mémoire (`metadata`), ou rien que les métriques
  (`off`) ; `full` est le défaut.
- `--mock-websocket relay|refuse` (`spec.mock.websocket`) : en mock, une poignée de main
  WebSocket sortante est relayée vers la vraie dépendance (`relay`, le défaut : les trames ne
  sont pas enregistrées, il n'y a rien à rejouer), ou refusée par le sidecar (`refuse`) :
  réponse `503` avec le header `x-mimic-websocket: refused`, sans aucune connexion vers la
  dépendance. Avec `refuse`, plus aucune connexion interceptée ne sort du pod en mock (le TCP
  non HTTP est déjà refusé par `mock.tcp`, réglage du sidecar). L'entrant n'est pas touché :
  l'application reçoit toujours ses WebSockets.

Un apply sans ces options les retire : `mimicctl mode checkout record` remet tout le
workload dans le même état.

```sh
mimicctl mode checkout mock --selector pod-template-hash=7d9f8c6b5 --capture-inbound metadata -n perf
mimicctl mode checkout mock --mock-websocket refuse -n perf   # isolement strict, WebSockets compris
mimicctl mode checkout record -n perf
```

### `status` : état des bascules

```sh
mimicctl status [<workload>] [-n ns | -A]
```

```
NAMESPACE   WORKLOAD   MODE   SNAPSHOT           UNMATCHED   ALIGNED
shop        checkout   mock   3f9a0c1e5b7d2a46   12          3/3
```

`UNMATCHED` compte les requêtes refusées par le mock (`599` sans enregistrement, `413` body
trop gros) depuis l'entrée en mock, sur les pods sélectionnés ; le détail est dans
`mimicctl report`. `ALIGNED` compte les pods qui appliquent l'état souhaité (mode, snapshot et capture) sur les
pods vus récemment, parmi les seuls pods sélectionnés quand la ressource a un `selector`. Un
mode refusé par le garde-fou production est affiché `mock (refusé)`. Juste
après une bascule, tant que le contrôleur n'a pas publié le statut de la nouvelle
génération, `SNAPSHOT` vaut `?` et `ALIGNED` `?/3` : attendre `3/3` avant de lancer un tir.

```sh
until mimicctl status checkout -n shop | tail -1 | grep -qE ' ([0-9]+)/\1$'; do sleep 2; done
```

### `export` : document OpenAPI découvert

```sh
mimicctl export <workload> (--inbound | --outbound <authority>) \
  [--openapi-version 3.1|3.0] [-o fichier] [-n ns]
```

`--inbound` donne l'API du service, `--outbound api.stripe.com:443` celle d'une dépendance,
par son authority `hôte:port` telle que la découverte la nomme. L'hôte seul
(`--outbound api.stripe.com`) suffit s'il n'a qu'une authority connue ; sinon la commande
échoue en listant les authorities connues.
Avec `-o`, l'extension choisit le format (`.yaml`/`.yml` : YAML, sinon JSON) ; sans `-o`,
JSON sur la sortie standard. `--openapi-version 3.0` convertit pour les outils qui ne lisent
pas encore 3.1.

```sh
mimicctl export checkout -n shop --inbound --openapi-version 3.0 -o checkout.yaml
mimicctl export checkout -n shop --outbound quote:8080 -o quote.json
```

### `requests` : requêtes enregistrées, pour le rejeu

```sh
mimicctl requests <workload> [--format json|jsonl|k6] [--limit n] [-n ns]
```

Requêtes entrantes enregistrées par les sidecars du workload (1000 au plus par défaut),
déjà redactées. `--format k6` produit un script k6 qui les rejoue :

```sh
mimicctl requests checkout -n shop --format k6 > replay.js
k6 run -e TARGET=http://checkout.shop:8080 -e VUS=20 -e ITERATIONS=5000 replay.js
```

Les headers redactés (jetons, cookies) ne sont pas rejoués ; `-e AUTHORIZATION=…` fournit
un `Authorization` valide pour le tir.

### `report` : rapport de découverte

```sh
mimicctl report [<workload>] [-n ns | -A] [--top n]
```

Markdown : pour chaque workload, son mode et ses pods, les API découvertes (le service puis
chaque dépendance) avec leurs opérations et appels, et les `n` opérations les plus appelées
(10 par défaut) avec codes, latences p50/p99 et headers d'une requête enregistrée, et les
requêtes sans réponse du mock (méthode, API, forme du chemin, `599` ou `413`, occurrences,
dernière occurrence). Fait pour le résumé d'un job GitHub Actions :

```sh
mimicctl report -n shop >> "$GITHUB_STEP_SUMMARY"
```

### `load` : tir de charge et dimensionnement

Rejoue les requêtes entrantes enregistrées contre le workload isolé (spec §5.9), sans k6.
Le workload doit être en `mock`, tous ses pods sélectionnés alignés (`mimicctl status`) :
sinon `load` refuse (code 2), car le tir frapperait les vraies dépendances.

```sh
# Palier fixe : 10 000 requêtes à une concurrence de 32.
mimicctl load checkout -n perf --target http://checkout.perf:8080 --concurrency 32 --requests 10000

# Montée 1, 2, 4… 256, 60 s par palier, arrêtée au premier palier saturé ; SLO p99 500 ms.
mimicctl load checkout -n perf --target http://checkout.perf:8080 --ramp --max-p99 500ms

# Depuis un fichier, sans cluster (aucune vérification, aucun relevé de ressources).
mimicctl requests checkout -n perf --format jsonl --limit 5000 > reqs.jsonl
mimicctl load --from reqs.jsonl --target http://localhost:8080 -H 'authorization: Bearer …'
```

La cible doit être joignable depuis le poste : un Service par `kubectl port-forward`, ou le
tir lancé depuis le cluster par le Job de `deploy/load/job.yaml` (l'image contient
`mimicctl`). Les requêtes partent en boucle fermée : `C` connexions persistantes, chacune
attend sa réponse avant la suivante. `--http2` parle HTTP/2 sans TLS, pour un service gRPC.

| Option | Défaut | Effet |
|--------|--------|-------|
| `--target URL` | requis | `http://hôte[:port][/préfixe]` ; `https` refusé |
| `--from fichier` | | échanges JSONL au lieu du contrôleur |
| `--limit n` | 1000 | requêtes lues sur le contrôleur |
| `-H, --header 'nom: valeur'` | | header ajouté ou remplacé, répétable (les headers redactés ne sont pas rejoués) |
| `--concurrency C` | 8 | palier fixe |
| `--requests N` / `--duration D` | 60s | fin du palier fixe : exactement `N` requêtes, ou une durée |
| `--ramp` | | montée par paliers |
| `--start`, `--factor`, `--max` | 1, 2, 256 | concurrences de la montée : `start`, ×`factor`, …, `max` |
| `--step-duration D` | 60s | durée d'un palier de la montée |
| `--max-error-rate` | 1% | saturé au-delà (erreurs : 5xx, délai, connexion) |
| `--max-p99 D` | | saturé au-delà |
| `--plateau` | 10% | saturé si le débit gagne moins que cette part de la hausse de concurrence (moins de +10 % en doublant) |
| `--timeout D` | 10s | délai d'une requête, réponse lue en entier |
| `--sample-interval D` | 15s | relevés `metrics.k8s.io` |
| `--pod-selector k=v` | | pods relevés, au lieu de ceux du contrôleur |
| `--cpu-headroom`, `--memory-headroom` | 30%, 20% | marges de la recommandation |
| `--format` | text | `text`, `markdown` (résumé de job GitHub) ou `json` |
| `--allow-live-dependencies` | | lève le refus hors mock |

Le rapport donne, par palier, le débit, les erreurs, p50/p95/p99/max, le CPU moyen et la
mémoire maximale des conteneurs de l'application (sans `mimic` ni les proxys des mesh), puis
le palier de capacité (le plus haut débit non saturé), la raison de la saturation et une
recommandation par pod : `requests.cpu` (CPU mesuré + marge, au millicœur), `requests.memory`
et `limits.memory` (mémoire maximale + marge, au MiB), débit par pod et par cœur. Exemple de
rapport (valeurs illustratives, deux pods derrière le Service) :

```text
palier  concurrence  durée   requêtes  débit/s  erreurs  p50      p95      p99      max      CPU/pod  mémoire/pod
1       1            60.0 s  9912      165.2    0.0 %    6.0 ms   6.9 ms   7.4 ms   21.1 ms  210m     182Mi
2       2            60.0 s  19604     326.7    0.0 %    6.1 ms   7.1 ms   7.9 ms   24.3 ms  395m     184Mi
3       4            60.0 s  36210     603.5    0.0 %    6.6 ms   8.0 ms   9.2 ms   30.2 ms  760m     187Mi
4       8            60.0 s  38001     633.4    0.0 %    12.6 ms  14.0 ms  15.1 ms  41.0 ms  795m     190Mi

Capacité : palier 3, concurrence 4, 603.5 req/s, p99 9.2 ms.
Saturation : palier 4, concurrence 8 : plateau, débit +5.0 % (seuil 10.0 %) pour une concurrence de 8 au lieu de 4.
Recommandation par pod (marges CPU 30.0 %, mémoire 20.0 %) : requests.cpu 988m, requests.memory et limits.memory 225Mi ; 301.8 req/s par pod, 397 req/s par cœur.
```

Lecture : entre 4 et 8 requêtes simultanées, la latence double et le débit ne bouge plus ; les
deux pods sont saturés vers 600 req/s, soit 300 par pod. Si le CPU moyen du palier de capacité
est proche de `limits.cpu`, c'est la limite qui sature : la relever et refaire le tir. Les
relevés de metrics-server sont des moyennes glissantes (15 à 30 s) : garder des paliers d'au
moins 60 s pour des chiffres de CPU fiables.

### `diff` : comparer deux découvertes

```sh
mimicctl diff <ancien> <nouveau> [--json] [--fail-on-breaking]
```

Opérations ajoutées ou supprimées, paramètres ajoutés, supprimés ou devenus requis, codes
de réponse ajoutés ou supprimés. Sont cassants : une opération supprimée, un paramètre
devenu requis, un paramètre requis ajouté, un code 2xx supprimé. En CI :

```sh
mimicctl export checkout -n shop --inbound -o cette-semaine.json
mimicctl diff semaine-derniere.json cette-semaine.json --fail-on-breaking
```

## Interface web du contrôleur

Ce que `export`, `requests` et `report` lisent est aussi visible dans l'interface du
contrôleur :

```sh
kubectl port-forward -n mimic-system "$(kubectl -n mimic-system get pod -l mimic.platform.io/leader=true -o name)" 8081
```

(la réplique active : `deploy/mimic-controller` peut désigner la passive, qui répond `503`).

`http://127.0.0.1:8081/` liste les workloads avec leur mode et leurs pods alignés ;
`/ui/<namespace>/<workload>` détaille un workload : pods et battements, API découvertes
(opérations, paramètres et leur caractère requis, codes, latences), liens vers les documents
OpenAPI, snapshots, dernières requêtes et exports pour le rejeu (k6, JSONL, JSON).

Chaque sidecar a aussi sa page d'admin, limitée à ce que ce pod a vu (sous contrôleur, aucun
sidecar n'importe le trafic des autres pods ; le workload entier est dans l'interface du
contrôleur) :
`kubectl port-forward -n shop pod/<pod> 15100` puis `http://127.0.0.1:15100/`.
