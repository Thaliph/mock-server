# Démo locale sur kind, pas à pas

Ce guide monte mimic sur un cluster kind local, déploie la démo OpenTelemetry, laisse mimic
découvrir les API des services, bascule `shipping` en mock, puis le ramène en `record`
(le mode « écoute » : le trafic est relayé vers les vraies dépendances et enregistré).

Chaque étape donne la commande, puis ce qu'il faut voir avant de passer à la suivante.
Les commandes sont celles de `scripts/e2e-kind.sh`, que la CI exécute sur chaque PR : pour
dérouler tout le parcours d'un coup, sans s'arrêter, lancer ce script (voir
[e2e-kind.md](e2e-kind.md)).

Toutes les commandes se lancent depuis la racine du dépôt.

## 0. Prérequis

| Outil | Version testée | Rôle |
|-------|----------------|------|
| Docker | 24+ | construire l'image, faire tourner le nœud kind |
| kind | 0.27 | cluster local (Kubernetes 1.32, native sidecars) |
| kubectl | 1.29+ | |
| helm | 3 | trust-manager |
| cargo | stable | construire `mimicctl` |
| jq, curl | | vérifications |

Prévoir 4 CPU et 8 Go de mémoire pour Docker, et une dizaine de Go de disque : les images de
la démo pèsent plusieurs Go. Le premier passage passe surtout son temps à les télécharger.

```sh
export CLUSTER=mimic IMAGE=mimic:dev NS=otel-demo
```

## 1. Créer le cluster kind

```sh
kind create cluster --name "$CLUSTER" --config tests/e2e/kind.yaml
kubectl cluster-info --context "kind-$CLUSTER"
```

À voir : `Kubernetes control plane is running at https://127.0.0.1:…`.

## 2. Construire l'image et la charger dans kind

Une seule image porte les cinq binaires (sidecar, init, contrôleur, webhook, `mimicctl`).

```sh
INIT_IMAGE=mimic-init:dev
docker build --target init -t "$INIT_IMAGE" .
docker build -t "$IMAGE" .
kind load docker-image "$IMAGE" "$INIT_IMAGE" --name "$CLUSTER"
```

À voir : `Image: "mimic:dev" … loading…`. Vérifier qu'elle est bien sur le nœud :

```sh
docker exec "$CLUSTER-control-plane" crictl images | grep mimic
```

Construire aussi `mimicctl` pour le poste :

```sh
cargo build --release -p mimicctl
export PATH="$PWD/target/release:$PATH"
mimicctl --version
```

Les manifests de `deploy/` visent les images publiées `ghcr.io/thaliph/mimic:0.1.0` et
`ghcr.io/thaliph/mimic-init:0.1.0`. On les applique en remplaçant ces noms par les images
locales :

```sh
manifest() {
  sed -e "s#ghcr.io/thaliph/mimic-init:0.1.0#$INIT_IMAGE#g" \
      -e "s#ghcr.io/thaliph/mimic:0.1.0#$IMAGE#g" "$@"
}
```

## 3. Installer le socle : cert-manager, trust-manager, contrôleur, webhook

### 3.1 cert-manager et trust-manager

cert-manager émet les certificats du contrôleur et du webhook ; trust-manager publie la CA
du contrôleur dans chaque namespace, pour que les sidecars lui fassent confiance.

```sh
kubectl apply -f https://github.com/cert-manager/cert-manager/releases/download/v1.21.1/cert-manager.yaml
kubectl -n cert-manager wait --for=condition=Available deploy --all --timeout=180s

kubectl apply -f deploy/controller/namespace.yaml
helm repo add jetstack https://charts.jetstack.io --force-update
helm upgrade --install trust-manager jetstack/trust-manager --version v0.24.0 \
  -n cert-manager --set app.trust.namespace=mimic-system --wait
```

### 3.2 Contrôleur

```sh
kubectl apply -f deploy/controller/crd.yaml -f deploy/controller/rbac.yaml \
  -f deploy/controller/production-guard.yaml
kubectl apply -f deploy/controller/certificates.yaml
kubectl -n mimic-system wait --for=condition=Ready certificate --all --timeout=120s
```

Si `certificates.yaml` est refusé juste après l'installation de cert-manager (`failed calling
webhook "webhook.cert-manager.io"`), son webhook n'est pas encore prêt : relancer la commande
quelques secondes plus tard.

Le nœud kind est seul et n'a pas de stockage partagé : on prend la variante `controller-rwo`
(PVC `ReadWriteOnce`, les deux répliques sur le nœud du volume), et on ramène la mémoire
demandée par chaque réplique de 1Gi à 256Mi pour que tout tienne.

```sh
kubectl kustomize deploy/controller-rwo \
  | manifest \
  | sed 's#memory: 1Gi$#memory: 256Mi#' \
  | kubectl apply -f -
kubectl -n mimic-system rollout status deploy/mimic-controller --timeout=180s
kubectl -n mimic-system get pods -l app.kubernetes.io/name=mimic-controller -L mimic.platform.io/leader
```

À voir : deux pods `Running`, dont **un seul** porte `LEADER=true` (la réplique active ;
l'autre attend en passive). Il peut falloir une quinzaine de secondes pour que la marque
apparaisse.

### 3.3 Webhook d'injection

Même raison que plus haut : on abaisse la requête CPU de chaque sidecar injecté à 10m. Et
la démo parle gRPC et texte : ces bodies, que la redaction ne sait pas lire, ne sont capturés
(et donc mockables) que par exception (`redact.raw_bodies`, ici pour tout).

```sh
manifest deploy/injector/injector.yaml \
  | sed 's#requests: {cpu: 50m, memory: 64Mi}#requests: {cpu: 10m, memory: 64Mi}#' \
  | sed 's#^      mock:$#      redact:\n        raw_bodies: ["*", inbound]\n      mock:#' \
  | kubectl apply -f -
kubectl -n mimic-system rollout status deploy/mimic-injector --timeout=180s
```

Le `caBundle` du webhook est posé par le cainjector de cert-manager ; attendre qu'il le soit
avant de créer des pods (sinon ils ne sont pas injectés, le webhook étant en
`failurePolicy: Ignore`) :

```sh
until [ -n "$(kubectl get mutatingwebhookconfiguration mimic-injector \
  -o jsonpath='{.webhooks[0].clientConfig.caBundle}')" ]; do sleep 3; done; echo caBundle posé
```

### 3.4 Vérifier les certificats

cert-manager peut réémettre la CA juste après sa création. Un certificat signé entre-temps
par l'ancienne CA reste `Ready`, mais les sidecars le refusent (`invalid peer certificate:
BadSignature`). Vérifier que les deux certificats ont été signés par la CA publiée :

```sh
ca=$(kubectl -n mimic-system get secret mimic-controller-ca -o jsonpath='{.data.tls\.crt}')
for s in mimic-controller-tls mimic-injector-tls; do
  [ "$ca" = "$(kubectl -n mimic-system get secret $s -o jsonpath='{.data.ca\.crt}')" ] \
    && echo "$s : ok" || echo "$s : ancienne CA"
done
```

Si l'un affiche `ancienne CA`, supprimer son Secret pour que cert-manager le réémette, puis
relancer la vérification (le contrôleur et le webhook relisent leur certificat toutes les
10 s) :

```sh
kubectl -n mimic-system delete secret mimic-controller-tls   # ou mimic-injector-tls
```

## 4. Déployer la démo

Le label `mimic.platform.io/enabled=true` sur le namespace active l'injection pour tous ses
pods. Il faut le poser **avant** de déployer.

```sh
kubectl create namespace "$NS"
kubectl label namespace "$NS" mimic.platform.io/enabled=true
```

Vérifier que le webhook injecte, sans rien créer (pod sonde en dry-run côté serveur) :

```sh
kubectl -n "$NS" run mimic-probe --image=busybox --restart=Never --dry-run=server \
  -o jsonpath='{.spec.initContainers[*].name}'; echo
```

À voir : `mimic-init mimic`. Si la sortie est vide, le webhook ne répond pas encore :
attendre quelques secondes et recommencer.

Déployer la démo OpenTelemetry (16 services et leur générateur de charge) et un pod `client`,
non injecté, d'où l'on enverra des requêtes :

```sh
kubectl apply -f tests/e2e/otel-demo.yaml -f tests/e2e/client.yaml
for d in $(kubectl -n "$NS" get deploy -o name); do
  kubectl -n "$NS" rollout status "$d" --timeout=600s
done
kubectl -n "$NS" wait --for=condition=Ready pod/client --timeout=120s
```

## 5. Onboarding : mimic sur les workloads

### 5.1 Chaque pod a son sidecar

```sh
for p in $(kubectl -n "$NS" get pods -o name); do
  printf '%-55s %s\n' "$p" "$(kubectl -n "$NS" get "$p" -o jsonpath='{.spec.initContainers[*].name}')"
done
```

À voir : `mimic-init mimic` pour chaque pod, sauf `client` et `valkey-cart-…`, qui portent
l'annotation `mimic.platform.io/inject: "false"`. `mimic` est un native sidecar (init
container `restartPolicy: Always`) : il démarre avant l'application et s'arrête après elle.

### 5.2 Le contrôleur voit tous les workloads, en `record`

Ouvrir l'interface du contrôleur sur sa réplique active, dans un second terminal (la commande
reste au premier plan) :

```sh
kubectl -n mimic-system port-forward \
  "$(kubectl -n mimic-system get pod -l mimic.platform.io/leader=true -o name)" 8081
```

Puis, dans le premier terminal :

```sh
curl -s http://127.0.0.1:8081/v1/workloads \
  | jq -r '.[] | select(.namespace == "otel-demo") | "\(.name)\t\([.pods[].mode] | join(","))"'
```

```
ad	record
cart	record
…
shipping	record,record
```

Chaque sidecar s'est annoncé au contrôleur et relaie le trafic en l'enregistrant : c'est le
mode par défaut, `record`. Les mêmes informations sont sur `http://127.0.0.1:8081/` (liste
des workloads, opérations découvertes, appels enregistrés).

`mimicctl status` ne liste encore rien : il affiche les ressources `MimicWorkload`, et il
n'en existe qu'à la première bascule.

### 5.3 Envoyer du trafic connu

Le générateur de charge de la démo envoie déjà du trafic. Pour un trafic qu'on maîtrise, on
demande des devis de livraison au frontend : `frontend` appelle `shipping` en gRPC, puis
`shipping` appelle `quote` en HTTP. C'est ce chemin qu'on mockera.

```sh
devis() {
  kubectl -n "$NS" exec client -- sh -c '
    ok=0
    for i in $(seq 1 '"$1"'); do
      curl -sf -m 10 -G http://frontend-proxy:8080/api/shipping \
        --data-urlencode "itemList=[{\"productId\":\"66VCHSJNUP\",\"quantity\":3}]" \
        --data-urlencode "currencyCode=USD" \
        --data-urlencode "address={\"streetAddress\":\"1600 Amphitheatre Parkway\",\"city\":\"Mountain View\",\"state\":\"CA\",\"country\":\"United States\",\"zipCode\":\"94043\"}" \
        | grep -q currencyCode && ok=$((ok + 1))
    done
    echo "$ok/'"$1"' devis OK"'
}
devis 40
```

À voir : `40/40 devis OK`. Les services de la démo n'ont pas de readiness probe : juste après
le déploiement, les premiers appels peuvent échouer. Relancer `devis 1` jusqu'à `1/1`.

### 5.4 Ce que mimic a découvert

L'API entrante de `shipping` (gRPC) et l'API de `quote` vue depuis `shipping` :

```sh
mimicctl export shipping -n "$NS" --inbound | grep GetQuote
mimicctl export shipping -n "$NS" --outbound quote:8080 | grep getquote
```

À voir : `/oteldemo.ShippingService/GetQuote` et `/getquote`. La découverte arrive au
contrôleur à chaque publication des sidecars : si rien ne sort, attendre une minute.

Le rapport de découverte de tout le namespace, en Markdown :

```sh
mimicctl report -n "$NS" > rapport.md
```

Les mêmes opérations s'affichent sur l'interface du contrôleur (`http://127.0.0.1:8081/`).

La boutique elle-même : `kubectl -n "$NS" port-forward svc/frontend-proxy 8080`, puis
`http://127.0.0.1:8080/`.

## 6. Activer le mock sur `shipping`, pas à pas

### 6.1 Noter les redémarrages avant la bascule

La bascule ne doit redémarrer aucun conteneur : on le vérifiera à la fin.

```sh
restarts() {
  kubectl -n "$NS" get pods -l app.kubernetes.io/name=shipping -o jsonpath='{range .items[*]}{.metadata.name}:{range .status.initContainerStatuses[*]}{.restartCount},{end}{range .status.containerStatuses[*]}{.restartCount},{end};{end}'
}
avant=$(restarts); echo "$avant"
```

### 6.2 Basculer

```sh
mimicctl mode shipping mock -n "$NS"
```

`mimicctl` crée ou met à jour la ressource `MimicWorkload` `shipping`. Le contrôleur fige un
snapshot des échanges enregistrés et le désigne aux deux pods ; chaque sidecar le télécharge
et passe en mock à son prochain battement, sans redémarrage.

```sh
kubectl -n "$NS" get mimicworkload shipping
mimicctl status shipping -n "$NS"
```

Juste après la bascule, `SNAPSHOT` vaut `?` et `ALIGNED` `?/2`. Attendre :

```sh
until mimicctl status shipping -n "$NS" | tail -1 | grep -qE 'mock +[0-9a-f]+ +2/2$'; do sleep 2; done
mimicctl status shipping -n "$NS"
```

```
NAMESPACE   WORKLOAD   MODE   SNAPSHOT           ALIGNED
otel-demo   shipping   mock   3f9a0c1e5b7d2a46   2/2
```

### 6.3 Couper la vraie dépendance

`shipping` ne doit plus avoir besoin de `quote` : on l'arrête.

```sh
kubectl -n "$NS" scale deploy/quote --replicas=0
kubectl -n "$NS" wait --for=delete pod -l app.kubernetes.io/name=quote --timeout=60s
devis 40
```

À voir : `40/40 devis OK`, alors que `quote` n'existe plus. Ce sont les sidecars de
`shipping` qui répondent, depuis le snapshot.

### 6.4 Le constater dans les métriques

`/metrics` du sidecar est lisible par le proxy de l'API Kubernetes :

```sh
metrics() {
  for p in $(kubectl -n "$NS" get pods -l app.kubernetes.io/name=shipping -o name); do
    kubectl get --raw "/api/v1/namespaces/$NS/pods/${p#pod/}:15100/proxy/metrics"
  done | grep -E "^$1"
}
metrics mimic_mock_responses_total
metrics mimic_mock_snapshot_exchanges
```

À voir : des `mimic_mock_responses_total{…}` qui augmentent à chaque `devis`, avec
`match` différent de `unmatched`. Des réponses `unmatched` (`599`) signaleraient un appel
absent du snapshot.

### 6.5 Aucun redémarrage

```sh
[ "$(restarts)" = "$avant" ] && echo "aucun redémarrage" || echo "REDÉMARRAGE : $avant -> $(restarts)"
```

## 7. Revenir en `record` (mode écoute)

C'est le retour à la normale : le sidecar relaie de nouveau vers la vraie dépendance et
reprend l'enregistrement.

### 7.1 Remettre la dépendance

```sh
kubectl -n "$NS" scale deploy/quote --replicas=1
kubectl -n "$NS" rollout status deploy/quote --timeout=120s
```

### 7.2 Basculer

```sh
mimicctl mode shipping record -n "$NS"
until mimicctl status shipping -n "$NS" | tail -1 | grep -qE 'record .* 2/2$'; do sleep 2; done
mimicctl status shipping -n "$NS"
```

```
NAMESPACE   WORKLOAD   MODE     SNAPSHOT   ALIGNED
otel-demo   shipping   record   -          2/2
```

### 7.3 Vérifier que le trafic repart vers `quote`

```sh
quote_requetes() {
  for p in $(kubectl -n "$NS" get pods -l app.kubernetes.io/name=quote -o name); do
    kubectl get --raw "/api/v1/namespaces/$NS/pods/${p#pod/}:15100/proxy/metrics"
  done | awk '/^mimic_requests_total[{]/ && /direction="inbound"/ { s += $NF } END { print s + 0 }'
}
mock_reponses() { metrics mimic_mock_responses_total | awk '{ s += $NF } END { print s + 0 }'; }

q0=$(quote_requetes); m0=$(mock_reponses)
devis 10
echo "requêtes reçues par quote : +$(( $(quote_requetes) - q0 ))"
echo "réponses du snapshot      : +$(( $(mock_reponses) - m0 ))"
```

À voir : `10/10 devis OK`, `quote` qui reçoit de nouveau les requêtes (`+10` environ), et
**`+0`** réponses du snapshot : `shipping` ne répond plus depuis le mock.

### 7.4 Toujours aucun redémarrage

```sh
[ "$(restarts)" = "$avant" ] && echo "aucun redémarrage" || echo "REDÉMARRAGE : $avant -> $(restarts)"
```

Les sidecars ont basculé deux fois (record, mock, record) sans qu'un seul conteneur ait
redémarré.

## 8. Pour aller plus loin

- **Mock de tout le namespace**, sauf le générateur de charge (c'est lui le client) :

  ```sh
  for w in $(kubectl -n "$NS" get deploy -o name | sed 's|.*/||' | grep -vxE 'load-generator|valkey-cart'); do
    mimicctl mode "$w" mock -n "$NS"
  done
  mimicctl status -n "$NS"
  ```

  Seuls `frontend-proxy` et `flagd`, que le générateur appelle directement, reçoivent encore
  du trafic ; tout le reste répond depuis les snapshots. Revenir avec la même boucle et
  `record`.

- **Pod neuf pendant le mock** : repasser `shipping` en mock et arrêter `quote` (étapes 6.2
  et 6.3), puis ajouter un réplica. Le nouveau pod reste quelques secondes en `Init:1/2` : son
  application n'est lancée qu'une fois que le sidecar a reçu l'état souhaité et chargé le
  snapshot (`startupProbe` sur `/readyz`).

  ```sh
  kubectl -n "$NS" scale deploy/shipping --replicas=3
  kubectl -n "$NS" get pods -l app.kubernetes.io/name=shipping -w   # Ctrl-C une fois Running
  until mimicctl status shipping -n "$NS" | tail -1 | grep -qE 'mock +[0-9a-f]+ +3/3$'; do sleep 2; done
  devis 40
  kubectl -n "$NS" scale deploy/shipping --replicas=2
  ```

  À voir : `ALIGNED` à `3/3` dès que le pod est `Running`, et `40/40 devis OK` alors que
  `quote` n'existe plus : le nouveau pod répond depuis le snapshot comme les deux autres
  (`metrics mimic_mock_responses_total`, étape 6.4). Revenir ensuite en `record` comme à
  l'étape 7.

- **Tir de charge** contre `shipping` en mock, avec dimensionnement CPU et mémoire :
  [guide/Tir-de-charge.md](guide/Tir-de-charge.md).

- **Garde-fou production** : dans un namespace labellisé `env=production`, `mimicctl mode …
  mock` est refusé (code de sortie 2), comme par le contrôleur et la politique d'admission.

## 9. Dépannage rapide

| Symptôme | Où regarder |
|----------|-------------|
| un pod n'a pas `mimic-init mimic` | webhook pas prêt à sa création : `kubectl -n "$NS" delete pod <pod>` le recrée injecté |
| `ALIGNED` reste à `0/2` | logs du sidecar : `kubectl -n "$NS" logs <pod> -c mimic --tail=50` ; `BadSignature` → étape 3.4 |
| `devis` échoue en mock | réponses `unmatched` dans `mimic_mock_responses_total` : le snapshot ne couvre pas l'appel, refaire du trafic en `record` avant de basculer |
| pods `Pending` (`Insufficient cpu`) | Docker n'a pas assez de CPU ou de mémoire : en donner davantage |

Le diagnostic complet est dans le [guide de dépannage](guide/Depannage.md).

## 10. Nettoyer

```sh
kind delete cluster --name "$CLUSTER"
```
