# Déploiement

## Contrôleur (`controller/`), une fois par cluster

| Fichier | Rôle |
|---------|------|
| `namespace.yaml` | namespace `mimic-system` |
| `crd.yaml` | CRD `MimicWorkload`, générée par `mimic-controller crd` |
| `rbac.yaml` | droits du contrôleur, rôles `mimic-operator` (bascule) et `mimic-viewer` |
| `certificates.yaml` | CA dédiée et certificat du contrôleur (cert-manager), CA publiée dans les namespaces (trust-manager) |
| `controller.yaml` | configuration, PVC, Deployment (deux réplicas, une active), Service, PodDisruptionBudget, NetworkPolicy |
| `production-guard.yaml` | facultatif (Kubernetes ≥ 1.30) : `ValidatingAdmissionPolicy` qui refuse une `MimicWorkload` en `mock` ou `mock-partial` dans un namespace labellisé `env=production` |

Prérequis : cert-manager et trust-manager, ce dernier avec `mimic-system` comme namespace
de confiance (`--set app.trust.namespace=mimic-system`), car la CA du contrôleur y vit. Ordre : `namespace`, `crd`, `rbac`,
`certificates`, puis `controller`.

Si la CA `mimic-controller-ca` est réémise (rotation, ou réémission par cert-manager juste
après sa création), cert-manager ne réémet pas les certificats qu'elle a déjà signés. Les
sidecars refusent alors le contrôleur (`invalid peer certificate: BadSignature` dans leurs
logs). Supprimer les Secrets `mimic-controller-tls` et `mimic-injector-tls`, ou lancer
`cmctl renew`, les fait réémettre avec la CA courante. Le contrôleur et le webhook relisent
leurs certificats au redémarrage : `kubectl -n mimic-system rollout restart deploy`.

### Haute disponibilité du contrôleur

Le contrôleur tourne en deux réplicas, dont **une seule est active** (spec §5.5.4). Elle est
élue par le bail `mimic-system/mimic-controller` (`kubectl -n mimic-system get lease`),
porte le label `mimic.platform.io/leader=true` et le Service ne route que vers elle. L'autre
est passive : prête (`/healthz`), elle ne lit ni n'écrit le support et répond `503` aux
sidecars et à l'interface. Les sidecars ne voient qu'une coupure de quelques secondes,
pendant laquelle ils gardent leur mode et leur snapshot ; un pod en mock continue de servir.

| Événement | Délai de bascule |
|-----------|------------------|
| arrêt propre (`kubectl delete pod`, mise à jour, drain) : bail rendu | moins de 10 s |
| arrêt brutal (nœud perdu, processus tué) : le bail doit expirer | jusqu'à 17 s de plus (`lease_duration` 15 s + `retry_period` 2 s) |

À la prise de relais, la nouvelle active recharge depuis le support l'état souhaité et le
snapshot courant de chaque workload, l'index des segments et les exemples par endpoint, puis
pose sa marque. **Ne se perd pas** : segments, snapshots, état souhaité, exemples, statut
des `MimicWorkload`, mock servi par les sidecars. **Se reconstruit** : état des pods (au
battement suivant, 2 s), découverte et formes de mock en cache (à la demande, identiques).
Après un arrêt brutal seulement, les exemples changés depuis la dernière passe de
maintenance (une minute) sont refaits depuis les segments, toujours sur le support.

**Stockage** : une seule réplique écrit, mais les deux doivent monter le support.

| Support | Ce qu'il faut |
|---------|---------------|
| `s3://…` (recommandé en production) | rien : supprimer le PVC et son montage, `store: s3://bucket/prefixe` |
| PVC `ReadWriteMany` (EFS, Filestore, Azure Files, CephFS, NFS…) | `accessModes: ["ReadWriteMany"]` et la `storageClassName` du stockage partagé : les deux réplicas peuvent être sur deux nœuds |
| PVC `ReadWriteOnce` (défaut des manifests) | suffit sur un nœud (kind) ou avec un volume local, qui garde les deux réplicas sur le même nœud : la passive couvre la perte du pod, pas celle du nœud. Avec un disque réseau sur plusieurs nœuds, une passive placée ailleurs reste en `ContainerCreating` (`Multi-Attach`) : passer à `ReadWriteMany` ou `s3://`, ou retirer l'anti-affinité |

Le Deployment se met à jour sans coupure du service des sidecars (`RollingUpdate`,
`maxUnavailable: 0`, l'active partant en dernier grâce à son `pod-deletion-cost`), le
PodDisruptionBudget (`minAvailable: 1`) empêche un drain d'évincer les deux réplicas à la
fois, et l'anti-affinité (préférentielle) les place sur deux nœuds quand c'est possible.
Métriques : `mimic_controller_leader` (1 sur l'active) et `mimic_controller_takeover_seconds`.
Droits ajoutés (`rbac.yaml`, Role dans `mimic-system`) : le bail, et `list`/`patch` sur les
pods pour la marque. Revenir à une seule réplique sans bail : `replicas: 1`, retirer
`leader_election` de la configuration et `mimic.platform.io/leader` du sélecteur du Service
(sans élection, aucune réplique ne porte la marque).

## Webhook d'injection (`injector/`), une fois par cluster

`injector.yaml` : configuration (image et configuration par défaut des sidecars),
certificat, Deployment, Service et `MutatingWebhookConfiguration`. À appliquer après
`controller/`, dont il réutilise l'Issuer `mimic-ca`.

Tout pod créé dans un namespace labellisé `mimic.platform.io/enabled=true` reçoit
`mimic-init` et le sidecar (spec §9.4). Annotations de pod reconnues :

| Annotation | Effet |
|------------|-------|
| `mimic.platform.io/inject: "false"` | pas d'injection |
| `mimic.platform.io/workload` | nom du workload (sinon label `app.kubernetes.io/name`, puis `app`) |
| `mimic.platform.io/inbound-ports` | ports entrants interceptés (défaut : ports TCP déclarés) |
| `mimic.platform.io/exclude-outbound-cidrs`, `…/exclude-outbound-ports` | destinations jamais redirigées |
| `mimic.platform.io/tls-intercept` | hôtes HTTPS interceptés ; l'app reçoit la CA Mimic |
| `mimic.platform.io/config` | ConfigMap de configuration du sidecar à la place de celle du webhook |
| `mimic.platform.io/mesh` | `auto` (défaut), `none`, `istio`, `istio-ambient` ou `linkerd` (spec §3.3) |

Le sidecar reçoit aussi `MIMIC_VERSION` (label `version_label` du pod,
`app.kubernetes.io/version` par défaut) et `MIMIC_POD_TEMPLATE_HASH` : toutes ses métriques
portent alors le label `version`, pour comparer deux versions d'un workload.

Le webhook est en `failurePolicy: Ignore` : s'il est indisponible, les pods sont créés
sans mimic. Un pod laissé tel quel l'est avec un avertissement visible dans `kubectl`.

### Pods dans un service mesh

Istio (sidecar ou ambient) et Linkerd sont pris en charge (spec §3.3) : mimic se place entre
l'application et le mesh et voit le HTTP en clair, le mesh garde son mTLS. Rien à
configurer : le webhook met mimic après les conteneurs du mesh, et `mimic-init` reconnaît
le mesh dans les règles iptables déjà posées (son log l'affiche : `mesh IstioSidecar …`).
Sous un mTLS strict, les sondes du sidecar passent par `exec`. Un namespace mis dans le mesh
(ou passé en ambient) après le démarrage de ses pods demande un `kubectl rollout restart`.
Le sidecar utilise l'UID 1338 et les ports 15100 (admin), 15101 et 15106, hors de ceux des
mesh.

## Application (`example/`)

| Fichier | Rôle |
|---------|------|
| `configmap.yaml` | configuration du sidecar (spec §9.1), rattaché au contrôleur |
| `deployment.yaml` | injection à la main : init container iptables, native sidecar, jeton projeté et CA du contrôleur |
| `deployment-injected.yaml` | le même workload avec le webhook : rien à ajouter au template |
| `mimicworkload.yaml` | état souhaité du workload : le seul moyen de le basculer |
| `vmpodscrape.yaml` | collecte des métriques par VictoriaMetrics |
| `networkpolicy-mock.yaml` | à appliquer pendant un tir en mode mock pour garantir qu'aucun appel ne sort |

Prérequis : Kubernetes 1.29 ou plus, le namespace labellisé `mimic.platform.io/enabled=true`
(pour recevoir la CA du contrôleur), un Secret `checkout-mimic` avec la clé `redact-salt`.

## Basculer un workload

Tous les pods basculent ensemble, sans redémarrage, et servent le même snapshot :

```sh
kubectl patch mimicworkload checkout -n shop --type merge -p '{"spec":{"mode":"mock"}}'
kubectl apply -f example/networkpolicy-mock.yaml
kubectl get mimicworkload -n shop   # snapshot servi, pods alignés
```

Rejouer un snapshot précis lors d'un tir suivant : `{"spec":{"mode":"mock","snapshot":"<id>"}}`.
Tous les pods servent ce même snapshot, poussé par le contrôleur : les sidecars ne s'échangent
pas leurs enregistrements. Les snapshots ne sont jamais supprimés ; l'historique des échanges
est de 24 heures (`retention` dans `controller.yaml`), et chaque endpoint du workload garde un
exemple rejouable au-delà (`<namespace>/<workload>/exemplars.jsonl` sur le PVC, au plus
`max_exemplars_per_workload` endpoints, spec §5.5.3). Leur taille est bornée :
`max_exemplar_bytes_per_workload` (64 MiB) par workload et `max_exemplar_bytes_total` (512 MiB)
en tout. Au-delà, un nouvel endpoint garde un exemple sans body (visible dans la découverte,
latence servie, pas rejouable) et un remplacement trop gros garde l'ancien exemple ; chaque cas
est compté dans `mimic_controller_exemplars_refused_total{reason="bytes"}`, la taille tenue
dans `mimic_controller_exemplars_bytes`. Prévoir cette mémoire dans la limite du contrôleur.
Le droit de basculer est le droit de modifier `mimicworkloads` (rôle `mimic-operator`).

**Garde-fou production** : dans un namespace labellisé `env=production`, le contrôleur ne
transmet jamais `mock` ni `mock-partial` aux sidecars (un workload déjà en mock repasse en
`record` dès que le label est posé) et l'indique sur la ressource : conditions `Aligned` et
`Allowed` à `False`, raison `ProductionNamespace`. `mimicctl mode` refuse aussi (code 2), et
`production-guard.yaml` refuse la ressource dès l'admission.
Les sidecars refusent tout autre ordre (`PUT /mode` répond `403`, l'annotation est ignorée).

## Voir ce que la découverte a trouvé

```sh
kubectl port-forward -n mimic-system "$(kubectl -n mimic-system get pod -l mimic.platform.io/leader=true -o name)" 8081
# puis http://127.0.0.1:8081/ : workloads, pods, API et paramètres requis,
# snapshots, requêtes enregistrées et export k6
```

La page d'admin d'un sidecar reste disponible pour déboguer un pod :
`kubectl port-forward deploy/checkout 15100:15100`. Hors `/healthz`, `/readyz` et `/metrics`,
ouverts au kubelet et à Prometheus, l'admin ne répond qu'aux connexions venues de
`127.0.0.1` ou `::1` (port-forward, `kubectl exec`) : `403` pour les autres pods, y compris
par le proxy de l'API Kubernetes (`kubectl get --raw …/pods/<pod>:15100/proxy/…`), qui n'obtient
que les métriques (spec §9.2).

### Limites du sidecar

Réglables dans la configuration du sidecar (`sidecar_config` du webhook, ou le ConfigMap monté
par l'annotation `mimic.platform.io/config`), spec §3.5 et §5.2 :

| Champ | Défaut | Effet |
|-------|--------|-------|
| `outbound.connect_timeout` | 10 s | établissement d'une connexion vers l'amont (DNS, TCP, TLS) ; au-delà, `504` et `mimic_upstream_errors_total{kind="connect_timeout"}`. Aucune limite sur la durée d'une réponse ou d'un flux |
| `server.header_read_timeout` | 60 s | en-têtes HTTP/1 d'une requête ; une connexion keep-alive inactive plus longtemps est refermée |
| `server.max_connections` | 2048 | connexions simultanées, entrant et sortant ensemble ; au-delà, refermées aussitôt et comptées dans `mimic_connections_refused_total` (l'admin a son propre plafond de 64) |
| `capture.max_inflight_bytes` | 64 MiB | bodies copiés en vol par la capture ; au-delà, bodies gardés tronqués (`reason="inflight_budget"`), sans effet sur le trafic |
| `share.max_snapshot_bytes` | 64 MiB | forme de mock téléchargée du contrôleur, compressée comme décompressée ; au-delà, le pod mocke sur ses données locales et l'erreur est dans son statut |

Une période nulle (`share.interval`, `share.mode_poll`) ou l'un de ces champs à 0 est refusé
au démarrage. La limite mémoire du sidecar doit couvrir au moins `capture.max_store_bytes` +
`share.max_backlog_bytes` + `capture.max_inflight_bytes` (144 MiB par défaut), plus environ
deux fois la forme de mock chargée : sous la limite de 256 Mi, une forme de quelques dizaines de
MiB. Sans Secret `<workload>-mimic` (clé `redact-salt`), le sidecar tire un sel au hasard et
l'annonce dans ses logs : les marqueurs de redaction diffèrent alors d'un pod à l'autre.

Si les noeuds utilisent iptables en mode legacy, ajouter `MIMIC_IPTABLES=iptables-legacy`
à l'init container.
