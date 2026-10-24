# Installer

[← Sommaire](README.md)

Installation du contrôleur et du webhook (une fois par cluster), puis activation par namespace.
Les manifests sont dans `deploy/` ; leur rôle est résumé dans `deploy/README.md`.

## Prérequis

| Élément | Exigence | Pourquoi |
|---------|----------|----------|
| Kubernetes | **1.29 ou plus** | le sidecar est un native sidecar (init container avec `restartPolicy: Always`) |
| Kubernetes, en option | 1.30 ou plus | `ValidatingAdmissionPolicy` du garde-fou production (`deploy/controller/production-guard.yaml`) |
| cert-manager | installé (testé en v1.21.1 par la CI) | CA du contrôleur, certificats du contrôleur et du webhook, `caBundle` du webhook (cainjector) |
| trust-manager | installé avec `app.trust.namespace=mimic-system` (testé en v0.24.0) | publie la CA du contrôleur (ConfigMap `mimic-controller-ca`) dans les namespaces labellisés `mimic.platform.io/enabled=true` |
| Helm 3 | pour trust-manager | |
| Capacités | `mimic-init` tourne en root avec `NET_ADMIN` et `NET_RAW` | règles iptables du pod. Le niveau `baseline` des Pod Security Standards n'autorise pas `NET_ADMIN` : les namespaces activés doivent l'admettre |
| Nœuds | `iptables` nft par défaut ; `iptables-legacy` réglable | voir `iptables` dans la [configuration de l'injecteur](Configurer.md#webhook-mimic-injector) |
| Stockage du contrôleur | PVC (10Gi dans le manifest, `ReadWriteOnce`) ou bucket S3 | segments, snapshots, état souhaité, exemples |
| Prometheus ou VictoriaMetrics | facultatif | voir [Métriques](Metriques.md) |

Versions vérifiées par la CI (`scripts/e2e-lib.sh`, `docs/e2e-kind.md`) : nœud kind
`kindest/node:v1.32.2`, cert-manager v1.21.1, trust-manager v0.24.0, Istio 1.30.6,
Linkerd `edge-26.10.1`.

## Image

Une seule image porte les quatre binaires (`mimic-proxy`, point d'entrée, `mimic-init`,
`mimic-controller`, `mimic-injector`) et tourne par défaut sous `1338:1338`.

```sh
docker build -t registry.interne/platform/mimic:0.1.0 .
docker push registry.interne/platform/mimic:0.1.0
```

Les manifests visent l'image publiée **`ghcr.io/thaliph/mimic:0.1.0`**, à quatre endroits :
l'image du Deployment du contrôleur, celle du Deployment du webhook, le champ `image` de la
configuration du webhook (image injectée dans les pods) et le Job de tir de charge. Pour
pointer vers un registre interne, les remplacer ensemble :

```sh
IMAGE=registry.interne/platform/mimic:0.1.0
for f in deploy/controller/controller.yaml deploy/injector/injector.yaml deploy/load/job.yaml; do
  sed -i "s#ghcr.io/thaliph/mimic:0.1.0#$IMAGE#g" "$f"
done
```

Même substitution sans modifier les fichiers (`manifest … | kubectl apply -f -`) : voir la
[démo locale sur kind](../demo-kind.md), qui charge une image construite localement.

## Ordre d'installation

### 1. cert-manager et trust-manager

```sh
kubectl apply -f https://github.com/cert-manager/cert-manager/releases/download/v1.21.1/cert-manager.yaml
kubectl -n cert-manager wait --for=condition=Available deploy --all --timeout=180s

# La CA du contrôleur vit dans mimic-system : c'est le namespace de confiance de trust-manager.
kubectl apply -f deploy/controller/namespace.yaml
helm repo add jetstack https://charts.jetstack.io --force-update
helm upgrade --install trust-manager jetstack/trust-manager \
  --version v0.24.0 -n cert-manager --set app.trust.namespace=mimic-system --wait
```

### 2. Contrôleur (`deploy/controller/`)

L'ordre est `namespace`, `crd`, `rbac`, `certificates`, puis `controller` ;
`production-guard.yaml` est facultatif (Kubernetes ≥ 1.30).

```sh
kubectl apply -f deploy/controller/crd.yaml -f deploy/controller/rbac.yaml
kubectl apply -f deploy/controller/production-guard.yaml        # facultatif
kubectl apply -f deploy/controller/certificates.yaml             # relancer si le webhook de cert-manager n'est pas encore prêt
kubectl -n mimic-system wait --for=condition=Ready certificate --all --timeout=120s
```

**Vérifier la CA du certificat.** cert-manager peut réémettre la CA juste après sa création ; un
certificat signé entre-temps reste `Ready` mais les sidecars le refusent
(`invalid peer certificate: BadSignature`). Comparer la CA qui a signé la feuille à la CA
courante, et supprimer le Secret de la feuille s'ils diffèrent :

```sh
ca=$(kubectl -n mimic-system get secret mimic-controller-ca -o jsonpath='{.data.tls\.crt}')
leaf=$(kubectl -n mimic-system get secret mimic-controller-tls -o jsonpath='{.data.ca\.crt}')
[ "$ca" = "$leaf" ] || kubectl -n mimic-system delete secret mimic-controller-tls   # cert-manager le réémet
```

Avant d'appliquer `controller.yaml`, choisir le support (voir
[Exploiter en production](Exploiter-en-production.md#stockage-du-contrôleur)) :

| Support | Modification de `controller.yaml` |
|---------|-----------------------------------|
| PVC `ReadWriteOnce` (défaut) | aucune ; suffit sur un nœud, ou si les deux réplicas restent sur le même nœud |
| PVC `ReadWriteMany` | `accessModes: ["ReadWriteMany"]` et la `storageClassName` du stockage partagé |
| S3 | `store: s3://bucket/prefixe` dans la ConfigMap, supprimer le PVC et son montage ; identifiants par les variables `AWS_*` (IRSA compris) |

```sh
kubectl apply -f deploy/controller/controller.yaml
kubectl -n mimic-system rollout status deploy/mimic-controller --timeout=180s
kubectl -n mimic-system get pods -l app.kubernetes.io/name=mimic-controller -L mimic.platform.io/leader
```

Une seule réplique doit porter `mimic.platform.io/leader=true`.

### 3. Webhook d'injection (`deploy/injector/injector.yaml`)

À appliquer après `controller/`, dont il réutilise l'Issuer `mimic-ca`. Avant, adapter la
ConfigMap `mimic-injector` :

- `exclude_outbound_cidrs` : **vide** dans le manifest livré, et c'est le cas courant. L'IP du
  Service `kubernetes` est exclue d'office : le webhook lit son propre
  `KUBERNETES_SERVICE_HOST` au démarrage et l'ajoute à la liste, l'API Kubernetes n'est donc
  jamais redirigée vers le sidecar. N'y mettre que ce qui doit rester réel pour **tous** les
  pods : un endpoint privé de l'API (EKS), une base de données partagée… Par pod, l'annotation
  `mimic.platform.io/exclude-outbound-cidrs` s'ajoute à cette liste ;
- `iptables: iptables-legacy` si les nœuds sont en iptables legacy ;
- `sidecar_config` : configuration par défaut des sidecars (voir [Configurer](Configurer.md)).

```sh
kubectl apply -f deploy/injector/injector.yaml
ca=$(kubectl -n mimic-system get secret mimic-controller-ca -o jsonpath='{.data.tls\.crt}')
leaf=$(kubectl -n mimic-system get secret mimic-injector-tls -o jsonpath='{.data.ca\.crt}')
[ "$ca" = "$leaf" ] || kubectl -n mimic-system delete secret mimic-injector-tls   # cert-manager le réémet
kubectl -n mimic-system rollout status deploy/mimic-injector --timeout=180s
# Le caBundle est posé par le cainjector : attendre qu'il soit là avant de créer des pods.
kubectl get mutatingwebhookconfiguration mimic-injector -o jsonpath='{.webhooks[0].clientConfig.caBundle}' | head -c 16; echo
```

Le webhook relit son certificat sur disque toutes les `tls.reload_interval` secondes (10 par
défaut) : une réémission de `mimic-injector-tls` est prise en compte sans redémarrage, le
temps que le kubelet rafraîchisse le volume (voir
[Exploiter en production](Exploiter-en-production.md#certificats)). En revanche sa
**configuration** (ConfigMap `mimic-injector`) est lue au démarrage : la modifier demande un
`rollout restart`.

## Activer l'injection

L'injection porte sur **les namespaces** labellisés `mimic.platform.io/enabled=true` (le
`namespaceSelector` du webhook). Le même label fait recevoir au namespace la ConfigMap
`mimic-controller-ca` (trust-manager), indispensable au sidecar. Il n'y a pas d'opt-in par pod
dans un namespace non labellisé.

```sh
kubectl label namespace shop mimic.platform.io/enabled=true
# Sel de redaction : facultatif sous contrôleur (il en tire un par workload) ; utile pour
# partager le sel entre clusters (voir Collecte et stockage)
kubectl -n shop create secret generic checkout-mimic --from-literal=redact-salt="$(openssl rand -hex 32)"
# Seuls les pods créés après le label sont injectés
kubectl -n shop rollout restart deploy/checkout
```

Réglages par pod (annotations du **template** de pod, lues à l'admission) :

| Annotation | Effet |
|------------|-------|
| `mimic.platform.io/inject: "false"` | pod laissé tel quel (opt-out) |
| `mimic.platform.io/workload` | nom du workload ; sinon label `app.kubernetes.io/name`, sinon `app` ; sans aucun des trois, le pod n'est pas injecté |
| `mimic.platform.io/inbound-ports` | ports entrants interceptés (`8080,9090`, `*`, ou `-` pour aucun) ; défaut : ports TCP déclarés des conteneurs, sinon `*` |
| `mimic.platform.io/exclude-outbound-cidrs` | CIDR sortants jamais redirigés, ajoutés à ceux de l'injecteur |
| `mimic.platform.io/exclude-outbound-ports` | ports sortants jamais redirigés |
| `mimic.platform.io/tls-intercept` | hôtes HTTPS interceptés (`api.stripe.com,*.salesforce.com`) ; l'app reçoit la CA mimic |
| `mimic.platform.io/config` | nom d'une ConfigMap (clé `mimic.yaml`) montée à la place de la configuration de l'injecteur |
| `mimic.platform.io/mesh` | `auto`, `none`, `istio`, `istio-ambient`, `linkerd` |

Référence complète : [Configurer](Configurer.md#annotations-de-pod).

Exemple minimal (`deploy/example/deployment-injected.yaml`) : le template reste celui de
l'application, avec au plus des annotations. `deploy/example/deployment.yaml` montre ce que le
webhook ajoute, écrit à la main.

## Vérifier que ça marche

```sh
# Le webhook injecte (pod d'essai en dry-run côté serveur) : doit afficher « mimic-init mimic »
kubectl -n shop run mimic-probe --image=busybox --restart=Never --dry-run=server \
  --labels app.kubernetes.io/name=mimic-probe -o jsonpath='{.spec.initContainers[*].name}'; echo

# Un pod réel : conteneurs injectés, annotation et label posés
POD=$(kubectl -n shop get pod -l app.kubernetes.io/name=checkout -o name | head -1)
kubectl -n shop get "$POD" -o jsonpath='{.spec.initContainers[*].name}{"\n"}{.metadata.annotations.mimic\.platform\.io/status}{"\n"}'
kubectl -n shop logs "$POD" -c mimic-init          # « mimic-init: mesh None, binaire iptables » puis « mimic-init: règles iptables posées »
kubectl -n shop logs "$POD" -c mimic | grep -E 'mimic démarré|battement'

# Le sidecar joint le contrôleur : controller.last_ok_ms renseigné, last_error à null
kubectl -n shop port-forward "$POD" 15100 &
curl -s http://127.0.0.1:15100/api/status | jq '.mode, .controller'

# Le contrôleur voit le workload
mimicctl report checkout -n shop
```

La page `http://127.0.0.1:15100/` du sidecar et l'interface du contrôleur
(`kubectl port-forward -n mimic-system "$(kubectl -n mimic-system get pod -l mimic.platform.io/leader=true -o name)" 8081`,
puis `http://127.0.0.1:8081/`) montrent ensuite les opérations découvertes. Un pod non injecté
porte un avertissement d'admission `mimic non injecté : <raison>` : voir [Dépannage](Depannage.md).

Pour piloter le workload, créer sa `MimicWorkload` (`deploy/example/mimicworkload.yaml`, ou
`mimicctl mode checkout record -n shop`).

## Désinstaller proprement

L'ordre évite des pods bloqués ou des appels réels inattendus :

```sh
# 1. Remettre les workloads dans le mode de leur configuration (record) et supprimer les ressources
kubectl get mimicworkloads -A
kubectl delete mimicworkloads --all -n shop                  # état souhaité vide : retour au mode de la configuration

# 2. Arrêter l'injection : d'abord le webhook (plus de nouveaux pods injectés)
kubectl delete mutatingwebhookconfiguration mimic-injector
kubectl label namespace shop mimic.platform.io/enabled-

# 3. Recréer les pods des workloads, qui repartent sans mimic-init ni sidecar
kubectl -n shop rollout restart deploy/checkout
kubectl -n shop delete secret checkout-mimic                 # si créé

# 4. Retirer le webhook et le contrôleur
kubectl delete -f deploy/injector/injector.yaml --ignore-not-found
kubectl delete -f deploy/controller/production-guard.yaml --ignore-not-found
kubectl delete -f deploy/controller/controller.yaml          # supprime aussi le PVC et ses données
kubectl delete -f deploy/controller/certificates.yaml -f deploy/controller/rbac.yaml
kubectl delete -f deploy/controller/crd.yaml                 # supprime toutes les MimicWorkload restantes
kubectl delete -f deploy/controller/namespace.yaml
```

- Sauvegarder d'abord le support si les snapshots ou les exemples doivent être gardés (voir
  [Exploiter en production](Exploiter-en-production.md#sauvegarde)).
- Les règles iptables vivent dans le namespace réseau du pod : elles disparaissent avec le pod,
  rien n'est à nettoyer sur les nœuds.
- Un pod encore injecté après le retrait du contrôleur continue de fonctionner dans son dernier
  mode : ses battements et publications échouent (logs), sans bloquer le trafic. Le recréer.
