# CLI mimicctl

[← Sommaire](README.md)

`mimicctl` pilote mimic depuis un poste ou une CI : bascule, statut, export OpenAPI, rejeu,
tir de charge, rapport et diff d'API. C'est la **référence unique** de la CLI (spec §9.5) ;
source : `crates/mimicctl/src/{main,lib,load,install}.rs`.

## Installation et accès

```sh
cargo install --path crates/mimicctl
mimicctl --version
```

`mimicctl` utilise le kubeconfig courant (`KUBECONFIG`, contexte, namespace par défaut) avec
les droits de son utilisateur :

| Commandes | Droits nécessaires |
|-----------|--------------------|
| `mode` | `create` et `patch` sur `mimicworkloads` du namespace, `get` sur le namespace (rôle `mimic-operator`) ; `list` sur `pods` du namespace pour vérifier que le workload existe |
| `status` | `list` sur `mimicworkloads` (rôle `mimic-viewer`, agrégé à `view`) |
| `emergency-stop` | `patch` sur le namespace visé, `mimic-system` avec `-A` (ClusterRole `mimic-emergency`) |
| `export`, `requests`, `report`, `snapshot ls`, `snapshot export` | `get` et `list` sur `workloads` et ses sous-ressources du groupe `data.mimic.platform.io`, **dans le namespace du workload** (ClusterRole `mimic-data-viewer` lié par RoleBinding) ; `report -A` : le même droit sur tout le cluster (ClusterRoleBinding) |
| `snapshot pin`, `snapshot import`, `snapshot gc` | `list` sur `pods` et `create` sur `pods/portforward` dans `mimic-system` (Role `mimic-cli`, exploitants) |
| `load <workload>` | ceux de `requests`, plus `get`/`list` sur `pods` de `metrics.k8s.io` dans le namespace du workload (relevés CPU et mémoire) |
| `load --from <fichier>` sans workload | aucun : ni cluster, ni vérification, ni relevé |
| `diff` | aucun : deux fichiers locaux |
| `install` | aucun pour écrire les manifests ; les appliquer demande les droits d'un administrateur du cluster (CRD, ClusterRole, webhook) |

**Lectures** (`export`, `requests`, `report`, `load`, `snapshot ls`, `snapshot export`) : par
l'API agrégée `data.mimic.platform.io/v1alpha1` (spec §9.2c), comme `kubectl top` passe par
metrics-server. L'API server authentifie l'utilisateur avec le kubeconfig (certificat, OIDC,
plugin `exec`…), le contrôleur vérifie ses droits sur le namespace et le workload demandés :
une équipe lit ses workloads, et rien d'autre. Si le groupe n'est pas servi (APIService
absente, cluster plus ancien), `mimicctl` se replie sur le port-forward.

**Écritures** (`snapshot pin|import|gc`) et repli : un port-forward vers le port 8081 d'un pod
du contrôleur, celui qui est en marche et porte `mimic.platform.io/leader=true`, sinon le
premier en marche. Comme `kubectl port-forward`, aucun port n'est exposé hors du cluster ;
l'interface ainsi ouverte voit **tous** les namespaces, d'où le Role `mimic-cli` réservé aux
exploitants. Pendant une bascule de réplique, la passive répond `503` : relancer la commande
quelques secondes plus tard.

`-n/--namespace` vaut par défaut le namespace du contexte ; `-A/--all-namespaces` couvre
tous les namespaces (`status` et `report`). Codes de sortie :

| Code | Sens |
|------|------|
| `0` | succès |
| `1` | `diff --fail-on-breaking` a trouvé un changement cassant |
| `2` | erreur ou refus (garde-fou production de `mode`, workload non isolé pour `load`) ; message sur la sortie d'erreur, préfixé `mimicctl: ` |

## `install`

```sh
mimicctl install [--storage shared|rwo|s3] [--image <registre>/mimic:<tag>] [--cni] [--production-guard]
```

Écrit sur la sortie standard les manifests du socle de la version de `mimicctl` (spec §9.5b),
embarqués dans le binaire : ni dépôt ni Rust nécessaires. Rien n'est appliqué : la sortie se
relit, se versionne (GitOps) ou se pipe dans `kubectl apply -f -`. Prérequis : cert-manager
et trust-manager installés.

```sh
mimicctl install | kubectl apply -f -
mimicctl install --storage s3 --image registry.example.com/team/mimic:1.2 > mimic.yaml
```

Ordre de la sortie, documents séparés par `---` : le contrôleur de la variante choisie
(`deploy/install/controller*.yaml`, rendus de `kubectl kustomize deploy/controller*` par
`scripts/render-install.sh`), le webhook (`deploy/injector/injector.yaml`, `inbound: false`
et ses quatre webhooks), puis, sur demande, le plugin CNI et le garde-fou production.

| Option | Effet |
|--------|-------|
| `--storage shared` | défaut : store sur un PVC `ReadWriteMany`, les deux répliques sur deux nœuds |
| `--storage rwo` | PVC `ReadWriteOnce` : répliques placées sur le nœud du volume, sans PodDisruptionBudget |
| `--storage s3` | store `s3://`, sans PVC (configurer le bucket et ses identifiants avant d'appliquer) |
| `--image <registre>/mimic:<tag>` | remplace `ghcr.io/thaliph/mimic:<version>` partout (contrôleur, webhook, DaemonSet CNI) ; l'image d'init devient `<registre>/mimic-init:<tag>`. Une image sans tag, ou dont le nom n'est pas `mimic`, est refusée (code 2) |
| `--cni` | ajoute `deploy/cni/mimic-cni.yaml` (DaemonSet `mimic-cni`) et `cni: true` dans la configuration du webhook (spec §3.1b) |
| `--production-guard` | ajoute `deploy/controller/production-guard.yaml` (`ValidatingAdmissionPolicy`, Kubernetes 1.30 ou plus) |

Une variante de `--storage` inconnue est refusée (code 2) en citant les variantes valides.

## `mode`

```sh
mimicctl mode <workload> <passthrough|record|mock|mock-partial> \
  [--snapshot <id>] [--selector k=v[,k=v…]] \
  [--capture-inbound full|metadata|off] [--mock-websocket relay|refuse] [--force] [-n ns]
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
| `--force` | | applique même sans pod injecté du workload (voir ci-dessous) |

Existence du workload : sans `MimicWorkload` de ce nom, `mimicctl` cherche dans le namespace
un pod injecté dont le sidecar porte `MIMIC_WORKLOAD=<workload>` (le nom retenu par le
webhook : annotation `mimic.platform.io/workload`, sinon label `app.kubernetes.io/name`, sinon
`app`). S'il n'y en a aucun, il refuse et liste les workloads injectés du namespace : une faute
de frappe ne crée plus une ressource qui ne pilote rien. `--force` passe outre (workload pas
encore déployé, ou à zéro réplica). Une `MimicWorkload` existante est toujours mise à jour.

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
| `ns/wl : ni MimicWorkload, ni pod injecté de ce workload ; workloads injectés de ns : …. --force pour appliquer quand même (workload pas encore déployé)` | nom mal écrit, ou workload pas encore déployé |
| `… aucun pod injecté dans ns (label mimic.platform.io/enabled=true du namespace ?)` | namespace sans injection, ou mauvais namespace (`-n`) |
| `pods de ns illisibles : existence de wl non vérifiée` | avertissement : pas de droit `list` sur les pods ; appliqué quand même |
| `mise à jour de ns/wl: …` | refus de l'API (droits, conflit de server-side apply avec un autre gestionnaire) |
| `kubeconfig: …` | pas de kubeconfig valide |

## `emergency-stop`

```sh
mimicctl emergency-stop [-n ns | -A] [--off]
```

Arrêt d'urgence (spec §5.5.1) : pose le label `mimic.platform.io/emergency-stop=true` sur le
namespace (celui du contexte par défaut), ou avec `-A` sur `mimic-system`, ce qui vaut pour
tout le cluster. `--off` retire le label.

| Effet | Délai |
|-------|-------|
| chaque pod concerné reçoit `passthrough` du contrôleur, quoi que demande sa `MimicWorkload` : plus de mock, plus d'enregistrement, trafic relayé tel quel | prochain battement (2 s) |
| conditions `Aligned` et `Allowed` à `False`, raison `EmergencyStop` (`mimicctl status`) | publication suivante du statut |
| nouveaux pods du namespace non injectés (le webhook l'exclut) ; avec `-A`, seuls les namespaces qui portent eux-mêmes le label | immédiat |
| `--off` : chaque workload reprend son mode souhaité, inchangé entre-temps | prochain battement |

Pour retirer aussi le sidecar des pods existants : `kubectl -n <ns> rollout restart
deploy/<workload>` après l'arrêt (les nouveaux pods ne sont plus injectés). Droit nécessaire :
`patch` sur le namespace visé (ClusterRole `mimic-emergency`, lié par RoleBinding dans le
namespace, ou par ClusterRoleBinding pour `-A`).

```sh
mimicctl emergency-stop -n shop
mimicctl emergency-stop -n shop --off
mimicctl emergency-stop -A
```

## `status`

```sh
mimicctl status [<workload>] [-n ns | -A]
```

```
NAMESPACE   WORKLOAD   MODE   SNAPSHOT           UNMATCHED   ALIGNED
shop        checkout   mock   3f9a0c1e5b7d2a46   12          3/3
```

| Colonne | Contenu |
|---------|---------|
| `MODE` | `spec.mode` ; suivi de `(refusé)` si `Allowed=False` |
| `SNAPSHOT` | `status.snapshot` ; `?` si le statut décrit une génération antérieure de la `spec` |
| `UNMATCHED` | `status.unmatched` : requêtes refusées par le mock (`599`, `413`) depuis l'entrée en mock, sommées sur les pods sélectionnés connus ; `0` avec un contrôleur qui ne le publie pas. Le détail est dans `mimicctl report` |
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
découvertes (`api.stripe.com:443`, `quote:8080`). L'hôte seul (`api.stripe.com`) suffit s'il
ne correspond qu'à une authority connue du workload ; sinon `mimicctl` sort en erreur (code 2)
avec la liste des authorities connues (`dépendance api.stripe.com inconnue ; authorities
connues : …`, ou `plusieurs dépendances pour l'hôte quote : quote:8080, quote:9090`). L'un des
deux est requis (`--inbound ou --outbound <authority> requis`). `-o/--output` : `.yaml`/`.yml`
en YAML, sinon JSON ; sans `-o`, JSON sur la sortie standard. `--openapi-version 3.0`
convertit (défaut `3.1`).

```sh
mimicctl export checkout -n shop --inbound --openapi-version 3.0 -o checkout.yaml
mimicctl export checkout -n shop --outbound quote:8080 -o quote.json
mimicctl export checkout -n shop --outbound api.stripe.com -o stripe.json   # seul port connu : 443
```

## `requests`

```sh
mimicctl requests <workload> [--direction inbound|outbound] [--format json|jsonl|k6] [--limit n] [-n ns]
```

Requêtes enregistrées, redactées ; 1000 par défaut. `--direction inbound` (défaut) : les
requêtes **reçues** par le service (capture `full`), à rejouer contre lui. `--direction
outbound` : les appels du service **à ses dépendances**, avec leurs réponses, tels que le mock
les rejouera : pour vérifier qu'un appel a bien été enregistré avant de basculer. `k6`
produit un script de rejeu, réglable par variables d'environnement :

| Variable k6 | Défaut | Effet |
|-------------|--------|-------|
| `TARGET` | `http://localhost:8080` | base des URL rejouées |
| `AUTHORIZATION` | vide | header `Authorization` à envoyer (les valeurs redactées ne sont pas rejouées) |
| `VUS` | `1` | utilisateurs virtuels |
| `ITERATIONS` | nombre de requêtes | itérations au total |

```sh
mimicctl requests checkout -n shop --direction outbound --limit 20
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
requêtes par sens). Le tableau récapitulatif donne aussi le total des requêtes sans réponse du
mock, et chaque workload a une section **Requêtes sans réponse du mock** : méthode, API, forme
du chemin (sans valeur), code (`599` ou `413`), occurrences et dernière occurrence (UTC), au
plus 50 clés, de la plus récente à la plus ancienne. C'est la première chose à lire après un
tir qui a reçu des `599`. Pour un résumé de job GitHub Actions :

```sh
mimicctl report -n shop >> "$GITHUB_STEP_SUMMARY"
```

## `load`

```sh
mimicctl load [<workload>] --target <url> [--from <fichier>] [--limit n] [-H 'nom: valeur']
  [--http2] [--concurrency C | --ramp] [--requests N | --duration D]
  [--start s] [--factor f] [--max m] [--step-duration D]
  [--max-error-rate r] [--max-p99 D] [--plateau r] [--timeout D]
  [--sample-interval D] [--pod-selector k=v] [--cpu-headroom r] [--memory-headroom r]
  [--format text|markdown|json] [--allow-live-dependencies] [-n ns]
```

Rejoue les requêtes entrantes enregistrées contre le workload **isolé de ses dépendances**
(spec §5.9), sans k6, et en déduit une taille de CPU et de mémoire par pod. Le workload doit
être en `mock` avec tous ses pods sélectionnés alignés : sinon `load` refuse (code 2), car le
tir frapperait les vraies dépendances. La conduite du tir et la lecture du rapport sont dans
[Tir de charge](Tir-de-charge.md) ; cette page ne liste que les options.

```sh
# Palier fixe : 10 000 requêtes à une concurrence de 32.
mimicctl load checkout -n perf --target http://checkout.perf:8080 --concurrency 32 --requests 10000

# Montée 1, 2, 4… 256, 60 s par palier, arrêtée au premier palier saturé ; SLO p99 500 ms.
mimicctl load checkout -n perf --target http://checkout.perf:8080 --ramp --max-p99 500ms

# Depuis un fichier, sans cluster (aucune vérification, aucun relevé de ressources).
mimicctl requests checkout -n perf --format jsonl --limit 5000 > reqs.jsonl
mimicctl load --from reqs.jsonl --target http://localhost:8080 -H 'authorization: Bearer …'
```

| Option | Défaut | Effet |
|--------|--------|-------|
| `--target URL` | requis | `http://hôte[:port][/préfixe]` ; `https` refusé |
| `--from fichier` | | échanges JSONL (`requests --format jsonl`) au lieu du contrôleur ; le workload devient facultatif |
| `--limit n` | 1000 | requêtes lues sur le contrôleur |
| `-H, --header 'nom: valeur'` | | header ajouté ou remplacé, répétable (les headers redactés ne sont jamais rejoués tels quels) |
| `--http2` | | HTTP/2 sans TLS, pour un service gRPC |
| `--concurrency C` | 8 | concurrence du palier fixe (exclusif avec `--ramp`) |
| `--requests N` | | fin du palier fixe : exactement `N` requêtes (exclusif avec `--duration` et `--ramp`) |
| `--duration D` | 60s | fin du palier fixe : une durée (exclusif avec `--ramp`) |
| `--ramp` | | montée par paliers jusqu'à saturation |
| `--start`, `--factor`, `--max` | 1, 2, 256 | concurrences de la montée : `start`, ×`factor`, …, `max` (demandent `--ramp`) |
| `--step-duration D` | 60s | durée d'un palier de la montée (demande `--ramp`) |
| `--max-error-rate r` | 1% | palier saturé au-delà (erreurs : 5xx, délai dépassé, échec de connexion) |
| `--max-p99 D` | aucun | palier saturé au-delà (votre SLO) |
| `--plateau r` | 10% | palier saturé si le débit gagne moins que cette part de la hausse de concurrence (moins de +10 % en doublant) |
| `--timeout D` | 10s | délai d'une requête, réponse lue en entier |
| `--sample-interval D` | 15s | intervalle des relevés `metrics.k8s.io` |
| `--pod-selector k=v` | | pods dont relever CPU et mémoire, au lieu de ceux du contrôleur |
| `--cpu-headroom r`, `--memory-headroom r` | 30%, 20% | marges de la recommandation |
| `--format` | text | `text`, `markdown` (résumé de job GitHub) ou `json` |
| `--allow-live-dependencies` | | lève le refus hors mock, en connaissance de cause |

Durées : `500ms`, `60s`, `2m`, `1h`. Proportions : `0.3` ou `30%`. La cible doit être joignable
depuis l'endroit où tourne `mimicctl` (`kubectl port-forward svc/checkout 8080`, ou le Job de
`deploy/load/job.yaml`, dont l'image contient `mimicctl`).

## `pull`

```sh
mimicctl pull <workload> [--snapshot <id>] [-o fichier] [-n ns]
```

Télécharge un snapshot pour le mock local (spec §9.7) : celui demandé, sinon celui que sert le
workload, sinon le plus récent. Écrit `<workload>-<id>.jsonl` par défaut (échanges redactés,
format de `snapshot export`), puis rappelle la commande qui le sert sans cluster. Mêmes droits
que `snapshot ls` et `snapshot export`.

```sh
mimicctl pull checkout -n shop
mimic-proxy --mock-from checkout-3f9a0c1e5b7d2a46.jsonl   # puis HTTP_PROXY=http://127.0.0.1:15101
```

Exemple complet, avec docker-compose : `deploy/local/`.

## `snapshot`

Cycle de vie des snapshots (spec §5.6) : écrits une fois (contenu immuable, métadonnées
versionnées), purgés au-delà de `snapshot.keep_last` sauf référencés et épinglés. `ls` et
`export` lisent par l'API agrégée, comme `export` et `requests` ; `pin`, `import` et `gc`
écrivent sur l'interface du contrôleur (port-forward, Role `mimic-cli`), jamais sur le mode ni
le trafic, qui restent pilotés par la ressource `MimicWorkload`.

```sh
mimicctl snapshot ls checkout -n shop
mimicctl snapshot pin checkout 3f9a0c1e5b7d2a46 -n shop
mimicctl snapshot pin checkout 3f9a0c1e5b7d2a46 --unpin -n shop
mimicctl snapshot export checkout 3f9a0c1e5b7d2a46 -o snapshot.jsonl -n shop
mimicctl snapshot import checkout snapshot.jsonl -n shop
mimicctl snapshot gc checkout -n shop
```

### `snapshot ls`

```sh
mimicctl snapshot ls <workload> [-n ns]
```

Tableau des snapshots du workload, du plus récent au plus ancien : `ID`, `CREATED_MS`,
`EXCHANGES`, `PINNED` (métadonnées, `?` si inconnues — snapshot d'une version antérieure du
contrôleur) et `REFERENCED` (vrai pour le `status.snapshot`, ou `spec.snapshot` à défaut, de
la `MimicWorkload` du workload).

### `snapshot pin`

```sh
mimicctl snapshot pin <workload> <id> [--unpin] [-n ns]
```

Épingle (protège de la purge, quel que soit son âge) ou, avec `--unpin`, désépingle un
snapshot. Identifiant invalide refusé avant tout appel. `404` si le snapshot n'existe pas.

### `snapshot export`

```sh
mimicctl snapshot export <workload> <id> [-o fichier] [-n ns]
```

Contenu JSONL complet du snapshot (même route que `GET …/snapshots/{id}`, lue par les
sidecars). Avec `-o`, écrit dans le fichier ; sans, sur la sortie standard.

### `snapshot import`

```sh
mimicctl snapshot import <workload> <fichier> [-n ns]
```

Importe un snapshot exporté ailleurs (par exemple d'un autre cluster ou d'une sauvegarde) :
contenu canonicalisé, **épinglé par défaut**, car il n'est référencé par aucun workload et
serait sinon purgé à la prochaine passe de maintenance. Fichier illisible refusé avant tout
appel ; contenu invalide refusé par le contrôleur (`400`).

### `snapshot gc`

```sh
mimicctl snapshot gc <workload> [-n ns]
```

Purge immédiate, hors des référencés, des épinglés et des `snapshot.keep_last` plus récents,
sans attendre la passe de maintenance (toutes les minutes). Affiche les identifiants
supprimés, un par ligne, ou « rien à purger ».

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
| `dépendance <x> inconnue ; authorities connues : …` | `export --outbound` : ni authority connue, ni hôte d'une seule authority | reprendre une des authorities listées |
| `plusieurs dépendances pour l'hôte <x> : …` | `export --outbound` avec un hôte qui a plusieurs ports | préciser `hôte:port` |
| `API server : 404 dépendance inconnue` (`contrôleur : 404 Not Found dépendance inconnue` en repli) | authority absente des API découvertes (contrôleur qui a perdu la dépendance entre la liste et l'export) | vérifier l'authority exacte (`hôte:port`) dans `mimicctl report` |
| `API server : 403 <utilisateur> ne peut pas get workloads/<sous-ressource> <ns>/<wl> (data.mimic.platform.io) : …` | pas de RoleBinding de `mimic-data-viewer` dans ce namespace | le demander à l'équipe plateforme ; `kubectl auth can-i get workloads.data.mimic.platform.io/requests -n <ns>` |
| `API server : 503 …` | APIService indisponible (contrôleur arrêté, certificat), ou `SubjectAccessReview` impossible | `kubectl get apiservice v1alpha1.data.mimic.platform.io` (condition `Available`) |
| `contrôleur : <code> <body>`, `API server : <code> <message>` | autre réponse du contrôleur | voir [Administration et API](Administration-et-API.md) |
| `pods du contrôleur: …`, `port-forward vers <pod>: …` | écriture de snapshot, ou repli sans API agrégée : droits `pods` `list` ou `pods/portforward` `create` manquants dans `mimic-system`, API injoignable | `kubectl auth can-i create pods/portforward -n mimic-system` |

## Interface web du contrôleur

Ce que `export`, `requests`, `report` et `load` lisent est aussi visible dans l'interface du
contrôleur, sur `ui_listen` (`127.0.0.1:8081`, port-forward seulement) :

```sh
kubectl port-forward -n mimic-system \
  "$(kubectl -n mimic-system get pod -l mimic.platform.io/leader=true -o name)" 8081
```

Viser la réplique **active** : `deploy/mimic-controller` peut désigner la passive, qui répond
`503`. `http://127.0.0.1:8081/` liste les workloads avec leur mode et leurs pods alignés ;
`/ui/<namespace>/<workload>` détaille un workload : pods et battements, API découvertes
(opérations, paramètres et leur caractère requis, codes, latences), liens vers les documents
OpenAPI, snapshots, requêtes sans réponse du mock, dernières requêtes et exports pour le rejeu
(k6, JSONL, JSON). Détail des routes : [Administration et API](Administration-et-API.md).

Chaque sidecar a aussi sa page d'admin, limitée à ce que **ce pod** a vu (sous contrôleur,
aucun sidecar n'importe le trafic des autres ; le workload entier est dans l'interface du
contrôleur) :

```sh
kubectl port-forward -n shop pod/<pod> 15100   # puis http://127.0.0.1:15100/
```
