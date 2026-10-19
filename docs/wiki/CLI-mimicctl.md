`mimicctl` pilote mimic depuis un poste ou une CI : bascule, statut, export OpenAPI, rejeu,
rapport et diff d'API. Source : `crates/mimicctl/src/{main,lib}.rs` ; guide d'origine :
`docs/cli.md`.

## Installation et accès

```sh
cargo install --path crates/mimicctl
mimicctl --version
```

`mimicctl` utilise le kubeconfig courant (`KUBECONFIG`, contexte, namespace par défaut) avec
les droits de son utilisateur :

| Commandes | Droits nécessaires |
|-----------|--------------------|
| `mode` | `create` et `patch` sur `mimicworkloads` du namespace, `get` sur le namespace (rôle `mimic-operator`) |
| `status` | `list` sur `mimicworkloads` (rôle `mimic-viewer`, agrégé à `view`) |
| `export`, `requests`, `report` | `list` sur `pods` et `create` sur `pods/portforward` dans `mimic-system` (`report` lit aussi `mimicworkloads`) |
| `diff` | aucun : deux fichiers locaux |

`export`, `requests` et `report` ouvrent un port-forward vers le port 8081 d'un pod du
contrôleur : celui qui est en marche et porte `mimic.platform.io/leader=true`, sinon le
premier en marche. Pendant une bascule de réplique, la passive répond `503` : relancer la
commande quelques secondes plus tard.

`-n/--namespace` vaut par défaut le namespace du contexte. Codes de sortie :

| Code | Sens |
|------|------|
| `0` | succès |
| `1` | `diff --fail-on-breaking` a trouvé un changement cassant |
| `2` | erreur ; message sur la sortie d'erreur, préfixé `mimicctl: ` |

## `mode`

```sh
mimicctl mode <workload> <passthrough|record|mock|mock-partial> \
  [--snapshot <id>] [--selector k=v[,k=v…]] \
  [--capture-inbound full|metadata|off] [--mock-websocket relay|refuse] [-n ns]
```

Crée ou met à jour la `MimicWorkload` du workload par server-side apply (gestionnaire
`mimicctl`). Les valeurs sont vérifiées **avant** tout appel au cluster ; un apply qui ne
reprend pas une option la retire (`mimicctl mode checkout record` remet tout à plat).

| Option | Champ | Effet |
|--------|-------|-------|
| `--snapshot <id>` | `spec.snapshot` | rejoue ce snapshot |
| `--selector k=v[,k=v…]` | `spec.selector.matchLabels` | seuls ces pods basculent ; les autres gardent le mode de leur configuration |
| `--capture-inbound` | `spec.capture.inbound` | capture de l'entrant pendant le tir |
| `--mock-websocket` | `spec.mock.websocket` | `refuse` : poignée de main WebSocket sortante refusée (`503`) en mock |

Garde-fou production : `mimicctl` lit les labels du namespace et refuse `mock`/`mock-partial`
dans un namespace `env=production`, sans rien appliquer. S'il ne peut pas les lire, il avertit
et applique ; le contrôleur refuse alors (`Allowed=False`, `ProductionNamespace`).

```sh
mimicctl mode checkout mock -n shop
mimicctl mode checkout mock --snapshot 3f9a0c1e5b7d2a46 -n shop
mimicctl mode checkout mock --selector pod-template-hash=7d9f8c6b5 --capture-inbound metadata -n perf
mimicctl mode checkout mock --mock-websocket refuse -n perf
mimicctl mode checkout record -n shop
```

Succès : `shop/checkout : mode souhaité mock`. Messages d'erreur :

| Message | Cause |
|---------|-------|
| `mode inconnu « X » ; modes valides : passthrough, record, mock, mock-partial` | mode mal écrit |
| `capture de l'entrant inconnue « X » ; valeurs valides : full, metadata, off` | `--capture-inbound` |
| `réglage WebSocket inconnu « X » ; valeurs valides : relay, refuse` | `--mock-websocket` |
| `sélecteur invalide « X » : attendu clé=valeur[,clé=valeur…]` | `--selector` |
| `namespace invalide : « X »` | nom de namespace |
| `le namespace X est labellisé env=production : le mode mock y est refusé (garde-fou production)` | garde-fou |
| `labels du namespace X illisibles (…) ; le contrôleur appliquera le garde-fou production` | avertissement : pas de droit `get` sur le namespace |
| `mise à jour de ns/wl: …` | refus de l'API (droits, conflit de server-side apply avec un autre gestionnaire) |
| `kubeconfig: …` | pas de kubeconfig valide |

## `status`

```sh
mimicctl status [<workload>] [-n ns | -A]
```

```
NAMESPACE   WORKLOAD   MODE   SNAPSHOT           ALIGNED
shop        checkout   mock   3f9a0c1e5b7d2a46   3/3
```

| Colonne | Contenu |
|---------|---------|
| `MODE` | `spec.mode` ; suivi de `(refusé)` si `Allowed=False` |
| `SNAPSHOT` | `status.snapshot` ; `?` si le statut décrit une génération antérieure de la `spec` |
| `ALIGNED` | `status.aligned/status.pods` (pods sélectionnés vus récemment) ; `?/n` tant que le statut n'est pas à jour |

Attendre l'alignement avant un tir :

```sh
until mimicctl status checkout -n shop | tail -1 | grep -qE ' ([0-9]+)/\1$'; do sleep 2; done
```

## `export`

```sh
mimicctl export <workload> (--inbound | --outbound <authority>) \
  [--openapi-version 3.1|3.0] [-o fichier] [-n ns]
```

Document OpenAPI découvert par le contrôleur (workload entier). `--inbound` : API du service ;
`--outbound <authority>` : celle d'une dépendance, telle qu'elle apparaît dans les API
découvertes (`api.stripe.com:443`, `quote:8080`). L'un des deux est requis
(`--inbound ou --outbound <authority> requis`). `-o` : `.yaml`/`.yml` en YAML, sinon JSON ;
sans `-o`, JSON sur la sortie standard. `--openapi-version 3.0` convertit (défaut `3.1`).

```sh
mimicctl export checkout -n shop --inbound --openapi-version 3.0 -o checkout.yaml
mimicctl export checkout -n shop --outbound quote:8080 -o quote.json
```

## `requests`

```sh
mimicctl requests <workload> [--format json|jsonl|k6] [--limit n] [-n ns]
```

Requêtes **entrantes** enregistrées (capture `full`), redactées ; 1000 par défaut. `k6`
produit un script de rejeu, réglable par variables d'environnement :

| Variable k6 | Défaut | Effet |
|-------------|--------|-------|
| `TARGET` | `http://localhost:8080` | base des URL rejouées |
| `AUTHORIZATION` | vide | header `Authorization` à envoyer (les valeurs redactées ne sont pas rejouées) |
| `VUS` | `1` | utilisateurs virtuels |
| `ITERATIONS` | nombre de requêtes | itérations au total |

```sh
mimicctl requests checkout -n shop --format k6 --limit 2000 > replay.js
k6 run -e TARGET=http://checkout.shop:8080 -e VUS=20 -e ITERATIONS=5000 -e AUTHORIZATION="Bearer …" replay.js
```

## `report`

```sh
mimicctl report [<workload>] [-n ns | -A] [--top n]
```

Rapport Markdown : par workload, mode et pods, API découvertes (le service puis chaque
dépendance) avec opérations et appels, et les `n` opérations les plus appelées (10 par
défaut) avec codes, latences p50/p99 et headers d'une requête enregistrée (lus sur 500
requêtes par sens). Pour un résumé de job GitHub Actions :

```sh
mimicctl report -n shop >> "$GITHUB_STEP_SUMMARY"
```

## `diff`

```sh
mimicctl diff <ancien> <nouveau> [--json] [--fail-on-breaking]
```

Compare deux documents OpenAPI (YAML ou JSON) : opérations ajoutées ou supprimées,
paramètres ajoutés, supprimés ou devenus requis, codes de réponse ajoutés ou supprimés.
Cassants : opération supprimée, paramètre devenu requis, paramètre requis ajouté, code 2xx
supprimé. Sans différence : `aucune différence`. `--json` : sortie JSON ;
`--fail-on-breaking` : code 1 s'il y a un changement cassant.

```sh
mimicctl export checkout -n shop --inbound -o cette-semaine.json
mimicctl diff semaine-derniere.json cette-semaine.json --fail-on-breaking
```

## Erreurs communes aux commandes qui lisent le contrôleur

| Message | Cause | Action |
|---------|-------|--------|
| `aucun pod du contrôleur en marche dans mimic-system` | contrôleur arrêté, ou pas de droit `list` sur les pods de `mimic-system` | `kubectl -n mimic-system get pods` |
| `contrôleur : 503 Service Unavailable réplique passive du contrôleur : …` | bascule de réplique en cours | relancer dans quelques secondes |
| `contrôleur : 404 Not Found dépendance inconnue` | authority absente des API découvertes | vérifier l'authority exacte (`hôte:port`) dans `/ui/<ns>/<wl>` ou `mimicctl report` |
| `contrôleur : <code> <body>` | autre réponse du contrôleur | voir [Administration et API](Administration-et-API) |
| `pods du contrôleur: …`, `port-forward vers <pod>: …` | droits `pods` `list` ou `pods/portforward` `create` manquants dans `mimic-system`, API injoignable | `kubectl auth can-i create pods/portforward -n mimic-system` |
