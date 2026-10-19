# Basculer en mock

[← Sommaire](README.md)

Isoler un workload de ses dépendances, vérifier que tous ses pods servent le même jeu, puis
revenir. Le comportement de chaque mode est décrit dans [Modes](Modes.md).

## Avant de basculer

| Vérification | Pourquoi | Comment |
|--------------|----------|---------|
| le workload a enregistré assez longtemps | le mock ne sert que des appels déjà vus ; une opération jamais enregistrée répond `599` | `mimicctl report <wl> -n <ns>`, ou la page `/ui/<ns>/<wl>` du contrôleur (API découvertes, nombre d'appels par opération) |
| les appels HTTPS à mocker sont interceptés **depuis l'enregistrement** | un HTTPS non intercepté n'est pas enregistré, et il est coupé en `mock` | annotation `mimic.platform.io/tls-intercept` ; la dépendance apparaît comme authority `hôte:443` dans les API découvertes |
| le namespace n'est pas labellisé `env=production` | `mock` et `mock-partial` y sont refusés | `kubectl get ns <ns> --show-labels` |
| les protocoles non HTTP sont pris en compte | ils restent joints réellement (bases, files de messages) | plan de tir |
| les pods sont vus par le contrôleur | un pod absent ne recevra pas l'état souhaité | `mimicctl status`, colonne `ALIGNED`, ou `kubectl get mw` |

## Basculer

### Avec `mimicctl` (recommandé)

```sh
mimicctl mode checkout mock -n shop
```

Crée ou met à jour la `MimicWorkload` `shop/checkout` par server-side apply (gestionnaire
`mimicctl`). Le mode est vérifié avant tout appel au cluster ; dans un namespace
`env=production`, la commande refuse sans rien appliquer :

```
mimicctl: le namespace shop est labellisé env=production : le mode mock y est refusé (garde-fou production)
```

Options (toutes facultatives) ; un apply qui ne les reprend pas les retire :

| Option | Champ | Effet |
|--------|-------|-------|
| `--snapshot <id>` | `spec.snapshot` | rejoue ce snapshot au lieu d'en construire un |
| `--selector k=v[,k=v…]` | `spec.selector.matchLabels` | seuls ces pods basculent |
| `--capture-inbound full\|metadata\|off` | `spec.capture.inbound` | allège la capture de l'entrant pendant le tir |
| `--mock-websocket relay\|refuse` | `spec.mock.websocket` | `refuse` : plus rien ne sort du pod en HTTP, WebSockets compris |

### Avec la ressource

```yaml
apiVersion: mimic.platform.io/v1alpha1
kind: MimicWorkload
metadata: {name: checkout, namespace: shop}
spec:
  mode: mock
  capture: {inbound: metadata}
```

```sh
kubectl apply -f checkout-mimic.yaml
# ou, sur une ressource existante :
kubectl -n shop patch mw checkout --type merge -p '{"spec":{"mode":"mock"}}'
```

Choisir un seul outil par ressource : un server-side apply sans forçage, comme celui de
`mimicctl`, échoue sur un conflit de propriétaire si le même champ a été posé par un autre
gestionnaire (`kubectl apply`, GitOps). Dans ce cas, modifier la ressource avec l'outil qui
la possède.

Droits : `mimic-operator` (tous les verbes sur `mimicworkloads`, `get` sur `namespaces`) pour
basculer ; `mimic-viewer`, agrégé à `view`, pour lire.

### Sans contrôleur

Un sidecar sans `share` vers un contrôleur se bascule par son API d'admin (voir
[Administration et API](Administration-et-API.md)) :

```sh
kubectl -n shop port-forward pod/checkout-7d9f8c6b5-abcde 15100
curl -X PUT --data mock http://127.0.0.1:15100/mode
```

Chaque pod est alors indépendant et sert ce que **lui** a enregistré.

## Ce qui se passe ensuite

1. Le contrôleur (réplique active) voit la ressource et calcule l'état souhaité. Sans
   `spec.snapshot`, il construit un snapshot des échanges sortants du workload (log
   `snapshot construit`, avec son identifiant et son nombre d'échanges), l'écrit sur le
   support et le publie dans `status.snapshot`.
2. Au battement suivant (toutes les 2 s), chaque pod sélectionné reçoit
   `{mode: mock, snapshot: <id>}`, télécharge la forme de mock (log `snapshot chargé`),
   construit son moteur, puis bascule (log `changement de mode`).
3. Le contrôleur compare ce qu'annonce chaque pod à l'état souhaité et met à jour le statut
   toutes les 5 s : `status.aligned` / `status.pods`, condition `Aligned`.

Délai habituel : quelques secondes. Si le téléchargement du snapshot échoue, le pod passe
**quand même** en mock, sur ses seules données locales (rien ne sort vers les dépendances), et
retente au battement suivant ; il reste `Diverging` tant qu'il n'annonce pas le bon snapshot.

## Vérifier l'alignement

```sh
mimicctl status checkout -n shop
```

```
NAMESPACE   WORKLOAD   MODE   SNAPSHOT           ALIGNED
shop        checkout   mock   3f9a0c1e5b7d2a46   3/3
```

- `?` dans `SNAPSHOT` et `ALIGNED` : le statut décrit encore l'ancienne génération de la
  `spec` ; attendre.
- `mock (refusé)` : garde-fou production (condition `Allowed=False`).
- **Ne lancer un tir qu'à `n/n`.** Attente scriptable :

```sh
until mimicctl status checkout -n shop | tail -1 | grep -qE ' ([0-9]+)/\1$'; do sleep 2; done
```

Avec kubectl :

```sh
kubectl -n shop get mw checkout
kubectl -n shop get mw checkout -o jsonpath='{range .status.conditions[*]}{.type}={.status} {.reason}{"\n"}{end}'
```

| `Aligned=False`, raison | Signification | Action |
|-------------------------|---------------|--------|
| `PodsDiverging` | au moins un pod n'annonce pas encore l'état souhaité | attendre quelques battements ; sinon regarder ses logs (`battement vers le contrôleur échoué`, `snapshot …`) et l'onglet pods de `/ui/<ns>/<wl>` |
| `NoPods` | aucun pod sélectionné vu depuis `pod_timeout` (30 s) | sélecteur trop restrictif, pods non injectés, ou sidecars qui n'atteignent pas le contrôleur |
| `SnapshotNotFound` | `spec.snapshot` inconnu sur le support ; l'état souhaité n'a pas changé | corriger l'identifiant (liste : `/v1/workloads/<ns>/<wl>/snapshots`) |
| `SnapshotFailed` | construction ou écriture du snapshot échouée (log `construction du snapshot échouée`) | état du support (PVC plein, droits S3) |
| `ProductionNamespace` | namespace `env=production` : l'état souhaité est `record` sans snapshot | voulu ; sinon retirer le label |

Côté métriques, pendant le tir : `mimic_mode{mode="mock"} == 1` sur chaque pod,
`mimic_mock_snapshot_exchanges > 0`, et le ratio de `mimic_mock_responses_total{match="unmatched"}`
(voir [Métriques](Metriques.md)).

## Basculer une partie des pods

```sh
mimicctl mode checkout mock --selector pod-template-hash=7d9f8c6b5 -n perf
```

Seuls les pods portant ces labels basculent ; les autres reçoivent un état vide et reviennent
au mode de **leur configuration** (`record` avec l'injecteur). Les labels comparés sont ceux
que le contrôleur lit dans l'API Kubernetes (gardés 60 s), jamais ce qu'annonce le sidecar.
`ALIGNED` ne compte que les pods sélectionnés. Un pod créé pendant le mock (mise à l'échelle,
redémarrage) qui porte les labels bascule à son premier battement.

## Garde-fou production

Jusqu'à quatre niveaux, du plus tôt au plus tard :

| Niveau | Effet |
|--------|-------|
| `mimicctl mode` | lit les labels du namespace et refuse `mock`/`mock-partial` (code 2). S'il ne peut pas les lire, avertit (`labels du namespace X illisibles (…) ; le contrôleur appliquera le garde-fou production`) et applique |
| admission (facultatif, `deploy/controller/production-guard.yaml`, Kubernetes ≥ 1.30) | la `ValidatingAdmissionPolicy` `mimic-production-guard` refuse la création ou la mise à jour : `mimic : mock et mock-partial sont refusés dans un namespace labellisé env=production` |
| réconciliation | `spec.mode` en mock dans un namespace `env=production` : état souhaité `record` sans snapshot, `Aligned=False` et `Allowed=False`, raison `ProductionNamespace` |
| battement | un pod d'un namespace de production ne reçoit jamais de mock ; s'il en annonce un (configuration, état rechargé du disque), il reçoit `record` |

Le label est surveillé (log `garde-fou production` à chaque changement) : poser
`env=production` sur un namespace en mock le fait revenir en `record` sans autre action.

## Revenir

```sh
mimicctl mode checkout record -n shop
```

Les pods quittent le mock au battement suivant ; le moteur de mock est libéré. Le snapshot
reste sur le support. Supprimer la ressource (`kubectl -n shop delete mw checkout`) a le même
effet : chaque pod revient au mode de sa configuration.

## Rejouer exactement le même jeu

Le snapshot est figé tant que le workload reste en `mock` ou `mock-partial`. Pour un second
tir identique après un retour en `record` :

```sh
SNAP=$(kubectl -n shop get mw checkout -o jsonpath='{.status.snapshot}')   # pendant le premier tir
mimicctl mode checkout mock --snapshot "$SNAP" -n shop                       # tir suivant
```

Sans `--snapshot`, une nouvelle entrée en mock construit un nouveau snapshot à partir des
données du moment (identifiant différent si les données ont changé).

## Points d'attention

- **Pods recréés pendant le mock.** Un pod neuf démarre dans le mode de sa configuration
  (`record`) jusqu'à son premier battement : les tout premiers appels de l'application peuvent
  atteindre les vraies dépendances. Si le contrôleur est injoignable à ce moment, le pod reste
  en `record` (son `emptyDir` ne contient aucun état souhaité). Éviter les redémarrages pendant
  un tir, ou vérifier `ALIGNED` après chacun.
- **Pod redémarré (conteneur seulement).** Le sidecar recharge du disque le dernier état
  souhaité et le snapshot (`dernier état souhaité rechargé`) : il reprend le mock même sans
  contrôleur.
- **Contrôleur indisponible pendant un tir.** Les pods gardent leur dernier état ; aucune
  bascule n'est possible tant qu'il n'est pas revenu (voir
  [Exploiter en production](Exploiter-en-production.md)).
- **`mock-partial` avec snapshot.** Une requête sans correspondance est relayée à chaque fois :
  le moteur ne s'enrichit pas pendant le tir. Les échanges relayés sont publiés et entreront
  dans le prochain snapshot.
- **Snapshot vide.** Un workload qui n'a rien enregistré en sortie obtient un snapshot vide :
  tous ses appels HTTP sortants répondent `599`.
