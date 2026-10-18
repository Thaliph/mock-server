# Déploiement

## Contrôleur (`controller/`), une fois par cluster

| Fichier | Rôle |
|---------|------|
| `namespace.yaml` | namespace `mimic-system` |
| `crd.yaml` | CRD `MimicWorkload`, générée par `mimic-controller crd` |
| `rbac.yaml` | droits du contrôleur, rôles `mimic-operator` (bascule) et `mimic-viewer` |
| `certificates.yaml` | CA dédiée et certificat du contrôleur (cert-manager), CA publiée dans les namespaces (trust-manager) |
| `controller.yaml` | configuration, PVC, Deployment, Service, NetworkPolicy |
| `production-guard.yaml` | facultatif (Kubernetes ≥ 1.30) : `ValidatingAdmissionPolicy` qui refuse une `MimicWorkload` en `mock` ou `mock-partial` dans un namespace labellisé `env=production` |

Prérequis : cert-manager et trust-manager, ce dernier avec `mimic-system` comme namespace
de confiance (`--set app.trust.namespace=mimic-system`), car la CA du contrôleur y vit. Ordre : `namespace`, `crd`, `rbac`,
`certificates`, puis `controller`.

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

Le sidecar reçoit aussi `MIMIC_VERSION` (label `version_label` du pod,
`app.kubernetes.io/version` par défaut) et `MIMIC_POD_TEMPLATE_HASH` : toutes ses métriques
portent alors le label `version`, pour comparer deux versions d'un workload.

Le webhook est en `failurePolicy: Ignore` : s'il est indisponible, les pods sont créés
sans mimic. Un pod laissé tel quel l'est avec un avertissement visible dans `kubectl`.

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
`max_exemplars_per_workload` endpoints, spec §5.5.3).
Le droit de basculer est le droit de modifier `mimicworkloads` (rôle `mimic-operator`).

**Garde-fou production** : dans un namespace labellisé `env=production`, le contrôleur ne
transmet jamais `mock` ni `mock-partial` aux sidecars (un workload déjà en mock repasse en
`record` dès que le label est posé) et l'indique sur la ressource : conditions `Aligned` et
`Allowed` à `False`, raison `ProductionNamespace`. `mimicctl mode` refuse aussi (code 2), et
`production-guard.yaml` refuse la ressource dès l'admission.
Les sidecars refusent tout autre ordre (`PUT /mode` répond `403`, l'annotation est ignorée).

## Voir ce que la découverte a trouvé

```sh
kubectl port-forward -n mimic-system deploy/mimic-controller 8081
# puis http://127.0.0.1:8081/ : workloads, pods, API et paramètres requis,
# snapshots, requêtes enregistrées et export k6
```

La page d'admin d'un sidecar reste disponible pour déboguer un pod :
`kubectl port-forward deploy/checkout 15000:15000`.

Si les noeuds utilisent iptables en mode legacy, ajouter `MIMIC_IPTABLES=iptables-legacy`
à l'init container.
