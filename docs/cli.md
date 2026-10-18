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
| `diff` | aucun : travaille sur deux fichiers |

`export`, `requests` et `report` ouvrent un port-forward vers le pod du contrôleur (port
8081), comme `kubectl port-forward` ; aucun port n'est exposé hors du cluster. Le pod choisi
est la réplique active, marquée `mimic.platform.io/leader=true` ; pendant une bascule, la
passive répond `503` et la commande est à relancer quelques secondes plus tard.

Les options `-n/--namespace` prennent le namespace du contexte par défaut. Codes de sortie :
`0` succès, `1` changement cassant détecté par `diff --fail-on-breaking`, `2` erreur
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
  dépendance. Avec `refuse`, plus rien ne sort du pod en mock. L'entrant n'est pas touché :
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
NAMESPACE   WORKLOAD   MODE   SNAPSHOT           ALIGNED
shop        checkout   mock   3f9a0c1e5b7d2a46   3/3
```

`ALIGNED` compte les pods qui appliquent l'état souhaité (mode, snapshot et capture) sur les
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

`--inbound` donne l'API du service, `--outbound api.stripe.com` celle d'une dépendance.
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
(10 par défaut) avec codes, latences p50/p99 et headers d'une requête enregistrée. Fait pour
le résumé d'un job GitHub Actions :

```sh
mimicctl report -n shop >> "$GITHUB_STEP_SUMMARY"
```

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
