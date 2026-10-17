# Déploiement

## Contrôleur (`controller/`), une fois par cluster

| Fichier | Rôle |
|---------|------|
| `namespace.yaml` | namespace `mimic-system` |
| `crd.yaml` | CRD `MimicWorkload`, générée par `mimic-controller crd` |
| `rbac.yaml` | droits du contrôleur, rôles `mimic-operator` (bascule) et `mimic-viewer` |
| `certificates.yaml` | CA dédiée et certificat du contrôleur (cert-manager), CA publiée dans les namespaces (trust-manager) |
| `controller.yaml` | configuration, PVC, Deployment, Service, NetworkPolicy |

Prérequis : cert-manager et trust-manager. Ordre : `namespace`, `crd`, `rbac`,
`certificates`, puis `controller`.

## Application (`example/`)

| Fichier | Rôle |
|---------|------|
| `configmap.yaml` | configuration du sidecar (spec §9.1), rattaché au contrôleur |
| `deployment.yaml` | init container iptables, native sidecar, jeton projeté et CA du contrôleur |
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
Le droit de basculer est le droit de modifier `mimicworkloads` (rôle `mimic-operator`).
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
