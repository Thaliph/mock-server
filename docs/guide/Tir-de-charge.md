# Tir de charge

[← Sommaire](README.md)

Mesurer la capacité d'un workload et dimensionner ses pods, sans toucher à ses dépendances :
on bascule le workload en `mock`, puis `mimicctl load` rejoue contre lui les requêtes
entrantes enregistrées, à concurrence fixe ou croissante jusqu'à saturation, et en déduit une
recommandation `requests`/`limits` CPU et mémoire. Référence : spec §5.8 et §5.9.

Le guide SRE détaillé (préparation, blue/green avec Argo Rollouts, tir k6) reste
`docs/sre-load-test.md` dans le dépôt.

## Principe

- **Les dépendances sont mockées** : les appels sortants HTTP du workload sont servis depuis
  le snapshot, avec leur latence enregistrée. Le tir mesure donc l'application seule.
- **Le trafic du tir est réel** : `mimicctl load` envoie les requêtes entrantes enregistrées
  (méthode, chemin, headers non redactés, body) au Service ou au pod. L'entrant n'est
  enregistré que si l'interception entrante est activée pour le workload (désactivée par
  défaut) : `kubectl label namespace perf mimic.platform.io/inbound=true`, puis recréer les
  pods (voir [Installer](Installer.md#activer-linjection)).
- **Boucle fermée** : `C` connexions persistantes ; chacune attend sa réponse avant d'envoyer
  la suivante. La concurrence est donc exactement `C` requêtes en vol.
- **Ressources** : pendant le tir, `mimicctl` relève CPU et mémoire des pods par
  `metrics.k8s.io` (metrics-server requis), conteneurs `mimic`, `istio-proxy` et
  `linkerd-proxy` exclus. Le premier tiers de chaque palier est ignoré (montée en régime).

## Préparer

```sh
# 1. le workload a enregistré assez de trafic (record), puis on l'isole
mimicctl mode checkout mock --capture-inbound metadata -n perf

# 2. attendre que tous les pods sélectionnés soient alignés
mimicctl status checkout -n perf        # ALIGNED doit afficher n/n
```

`mimicctl load` vérifie lui-même l'isolement : si le workload n'est pas en `mock` ou si un pod
n'est pas aligné, il refuse avec le code de sortie `2`, car le tir frapperait les vraies
dépendances. `--allow-live-dependencies` lève ce refus, en connaissance de cause.

À savoir avant de tirer (voir [Modes](Modes.md#trafic-sortant)) :

- les connexions TCP non HTTP (bases, files de messages) sont **refusées** en `mock`
  (`mock.tcp: refuse`, le défaut) : l'application les voit coupées. Un service qui ne répond
  plus sans sa base fausse le tir : soit l'exclure de l'interception
  (`mimic.platform.io/exclude-outbound-ports`) pour qu'elle reste réelle et reçoive la charge,
  soit accepter l'erreur. `mimic_passthrough_connections_total{kind="tcp",result="refused"}`
  doit rester à zéro si le service n'est pas censé en ouvrir ;
- un HTTPS sortant non intercepté est coupé en `mock` ;
- une connexion WebSocket sortante est relayée, sauf `--mock-websocket refuse` ;
- `mimic_mock_responses_total{match="unmatched"}` (réponses `599`) doit rester proche de zéro :
  sinon le snapshot ne couvre pas le trafic rejoué et les latences mesurées sont fausses ;
- un pod neuf pendant le tir (autoscaling, redéploiement) démarre en mock sur le snapshot :
  son application attend l'état souhaité. Seul un contrôleur injoignable plus de
  `share.startup.timeout` le fait démarrer dans le repli, `record` par défaut (voir
  [Basculer en mock](Basculer-en-mock.md#pods-neufs-pendant-un-mock)) ; le pod reste quelques
  secondes en `Init`, ce qui retarde d'autant sa prise de charge par l'autoscaling.

## Tirer

### Palier fixe : N appels à une concurrence cible

```sh
mimicctl load checkout -n perf --target http://checkout.perf:8080 \
  --concurrency 32 --requests 10000
```

`--requests N` envoie exactement `N` requêtes ; `--duration 5m` tire pendant une durée. Les
requêtes enregistrées sont rejouées en boucle si `N` dépasse leur nombre (`--limit`, 1000 par
défaut, en lit davantage sur le contrôleur).

### Montée progressive jusqu'à saturation

```sh
mimicctl load checkout -n perf --target http://checkout.perf:8080 \
  --ramp --start 1 --factor 2 --max 256 --step-duration 60s --max-p99 500ms
```

Les paliers de concurrence sont `start`, ×`factor`, …, jusqu'à `max` (ici 1, 2, 4… 256). Le
tir s'arrête au **premier palier saturé**. Un palier est saturé quand :

| Raison | Condition | Défaut |
|--------|-----------|--------|
| erreurs | taux d'erreurs (5xx, délai dépassé, échec de connexion) au-delà de `--max-error-rate` | 1 % |
| latence | p99 au-delà de `--max-p99` (votre SLO) | aucun |
| plateau | le débit gagne moins que `--plateau` de la hausse de concurrence | 10 % (moins de +10 % en doublant) |

La **capacité** est le palier non saturé de plus haut débit.

### Depuis le cluster

La cible doit être joignable depuis l'endroit où tourne `mimicctl`. Le plus simple est de
lancer le tir dans le cluster avec `deploy/load/job.yaml` : l'image `ghcr.io/thaliph/mimic`
contient `mimicctl`, le Job a les droits de lecture `metrics.k8s.io` dans le namespace du
workload et la lecture de ses données (`mimic-data-viewer`, lié dans ce seul namespace). Adapter le nom du workload, le namespace et la
cible dans `args`, puis :

```sh
kubectl apply -f deploy/load/job.yaml
kubectl logs -n perf job/checkout-load -f
```

Depuis un poste, passer par `kubectl port-forward svc/checkout 8080 -n perf` et
`--target http://localhost:8080`.

### Sans cluster

```sh
mimicctl requests checkout -n perf --format jsonl --limit 5000 > reqs.jsonl
mimicctl load --from reqs.jsonl --target http://localhost:8080 -H 'authorization: Bearer …'
```

Ni vérification d'isolement ni relevé de ressources : seulement débit et latences. `-H`
ajoute ou remplace un header, utile pour les headers redactés (jamais rejoués tels quels).

## Lire le rapport

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

(Valeurs illustratives, deux pods derrière le Service.)

- **Entre les paliers 3 et 4**, la concurrence double mais le débit ne gagne que 5 % et la
  latence double : les requêtes font la queue. Les deux pods plafonnent vers 600 req/s,
  soit 300 par pod.
- **Recommandation** : `requests.cpu` = CPU moyen du palier de capacité + 30 %, arrondi au
  millicœur ; `requests.memory` = `limits.memory` = mémoire maximale + 20 %, arrondie au MiB.
  Les marges se règlent par `--cpu-headroom` et `--memory-headroom`.
- **Débit par pod et par cœur** : pour dimensionner le nombre de réplicas
  (`débit cible / débit par pod`) ou un HPA.

Pièges de lecture :

- si le CPU du palier de capacité est proche de `limits.cpu`, c'est le throttling qui sature :
  relever la limite et refaire le tir ;
- metrics-server donne des moyennes glissantes de 15 à 30 s : garder des paliers d'au moins
  60 s, sinon les chiffres de CPU sont sous-estimés ;
- une saturation par **erreurs** dès les premiers paliers signale plutôt un problème
  fonctionnel (headers d'authentification redactés, `599` du mock) qu'une limite de capacité :
  regarder les codes dans les logs de l'application.

## Intégrer à une CI

`--format markdown` produit un tableau à coller dans le résumé d'un job GitHub
(`>> "$GITHUB_STEP_SUMMARY"`), `--format json` le rapport complet pour un traitement
automatique. Codes de sortie : `0` tir terminé, `2` erreur ou workload non isolé.

## Revenir

```sh
mimicctl mode checkout record -n perf
```

Avec `--capture-inbound metadata`, le tir n'a rien ajouté au store : le snapshot suivant reste
représentatif du trafic réel.
