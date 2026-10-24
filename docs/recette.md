# Recette des points 6 à 12, pas à pas

Ce document vérifie à la main ce qui a été livré du 23 au 24 octobre 2026 (PR #42 à #49, spec
v0.22 à v0.29), dans l'ordre où cela se teste le plus simplement. Chaque étape donne les
commandes et ce qu'il faut voir. Une étape qui échoue : noter la sortie, passer à la suivante.

| Partie | Où | Durée |
|--------|----|-------|
| A. Sur le poste, sans cluster | dépôt cloné | 15 min |
| B. Socle sur kind | [démo kind](demo-kind.md), sections 0 à 5 | 30 min (téléchargements) |
| C. Fonctions sur le cluster | même cluster | 45 min |
| D. Nettoyage | | 2 min |

Points jamais exécutés pendant le développement, à regarder en priorité : **A2** (promtool),
**A3** (rendu kustomize), **A5** (docker compose), **C3** (API agrégée réelle), **C9**
(variante CNI), **C10** (`mimicctl pull` contre un vrai contrôleur).

---

## A. Sur le poste, sans cluster

### A1. Construction et tests

```sh
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast 2>&1 | grep -E "^test result|FAILED"
```

À voir : aucune sortie de `fmt` ni de `clippy` ; environ 516 tests passés, 11 ignorés. Trois
tests sensibles au temps peuvent échouer sur une machine chargée (`mck_15`, `lod_11`,
`prx_11`) : les relancer seuls (`cargo test -p mimic-proxy --test mock_latency`), ils
doivent passer machine au repos.

### A2. Règles d'alerte avec promtool (MON-07, jamais exécuté)

```sh
docker run --rm -v "$PWD:/w" -w /w --entrypoint /bin/sh prom/prometheus:v3.5.0 scripts/check-monitoring.sh
```

À voir : `SUCCESS: 34 rules found` puis `SUCCESS` pour les tests unitaires. Un échec de
`promtool test rules` affiche l'alerte, l'instant et l'écart (`exp:` / `got:`) : c'est le
fichier `deploy/monitoring/tests.yaml` qu'il faudra corriger.

### A3. Variantes du contrôleur (CTL-50, rendu jamais exécuté)

```sh
kubectl kustomize deploy/controller | grep -A3 "kind: PersistentVolumeClaim" | grep -A1 accessModes
kubectl kustomize deploy/controller-rwo > /tmp/rwo.yaml
grep -c "kind: PodDisruptionBudget" /tmp/rwo.yaml
grep -A6 "podAffinity:" /tmp/rwo.yaml
grep -A1 accessModes /tmp/rwo.yaml
kubectl kustomize deploy/controller-s3 > /tmp/s3.yaml
grep "store:" /tmp/s3.yaml; grep -c "kind: PersistentVolumeClaim" /tmp/s3.yaml
grep -B1 -A2 "name: data" /tmp/s3.yaml | grep -E "emptyDir|persistentVolumeClaim"
```

À voir :
- base : `ReadWriteMany` ;
- `rwo` : `0` PodDisruptionBudget, un `podAffinity` `requiredDuringScheduling…` sur
  `kubernetes.io/hostname`, plus de `podAntiAffinity`, `ReadWriteOnce` ;
- `s3` : `store: s3://mimic-controller/`, `0` PVC, volume `data` en `emptyDir: {}` ;
- aucune erreur de kustomize (`$patch: delete`, correctif JSON de la ConfigMap).

### A4. Mock local avec le binaire (PRX-26)

```sh
cargo build --release -p mimic-proxy -p mimicctl
export PATH="$PWD/target/release:$PATH"
mimic-proxy --config deploy/local/mimic.yaml --mock-from deploy/local/snapshot.jsonl &
http_proxy=http://127.0.0.1:15101 curl -si "http://quotes.internal:8080/quote?zip=75001"
http_proxy=http://127.0.0.1:15101 curl -s -o /dev/null -w "%{http_code}\n" http://quotes.internal:8080/autre
kill %1
```

À voir : `200`, `x-mimic-match: exact;score=1.00`, `{"price":42,"currency":"EUR"}` ; puis
`599`. Sans `--mock-from` et sans sel, le sidecar doit refuser de démarrer :
`mimic-proxy` seul → `mimic: redact.salt requis sans contrôleur…`, code de sortie 2.

### A5. Mock local avec docker compose (DOC-09, jamais exécuté)

```sh
docker build -t ghcr.io/thaliph/mimic:0.1.0 .      # image locale sous le nom attendu
cd deploy/local && docker compose up               # Ctrl-C pour arrêter
```

À voir, toutes les 5 s dans les logs de `app` : `HTTP/1.1 200 OK`, `x-mimic-match: exact`,
`{"price":42,"currency":"EUR"}`. `curl -s localhost:15100/metrics | grep mimic_build_info`
depuis l'hôte : une ligne `mimic_build_info{mimic_version="0.1.0",…} 1`.

---

## B. Socle sur kind

Suivre la [démo kind](demo-kind.md), **sections 0 à 5** (cluster, image, cert-manager,
contrôleur, webhook, démo OpenTelemetry, trafic, découverte). Deux choses ont changé et sont
déjà dans la démo :
- le contrôleur s'installe avec `kubectl kustomize deploy/controller-rwo` (kind n'a
  pas de stockage partagé) ;
- le webhook reçoit `redact.raw_bodies: ["*", inbound]` (la démo parle gRPC, dont les bodies
  ne seraient sinon pas capturés).

Garder pour la suite, dans le même shell, les variables et les fonctions `manifest` et
`devis` de la démo :

```sh
export CLUSTER=mimic IMAGE=mimic:dev NS=otel-demo
export PATH="$PWD/target/release:$PATH"
type manifest devis >/dev/null && echo prêt
```

À voir en fin de section 5 : `devis 40` → `40/40 devis OK`, et les exports de 5.4.

---

## C. Fonctions sur le cluster

Interface du contrôleur dans un second terminal, quand une étape la demande :

```sh
kubectl -n mimic-system port-forward "$(kubectl -n mimic-system get pod -l mimic.platform.io/leader=true -o name)" 8081
```

### C1. Haute disponibilité en ReadWriteOnce (CTL-50)

```sh
kubectl -n mimic-system get pods -l app.kubernetes.io/name=mimic-controller -o wide -L mimic.platform.io/leader
kubectl -n mimic-system get pdb
kubectl -n mimic-system delete "$(kubectl -n mimic-system get pod -l mimic.platform.io/leader=true -o name)"
sleep 25; kubectl -n mimic-system get pods -l app.kubernetes.io/name=mimic-controller -L mimic.platform.io/leader
kubectl -n mimic-system rollout restart deploy/mimic-controller
kubectl -n mimic-system rollout status deploy/mimic-controller --timeout=180s
```

À voir : deux pods `Running` sur le même nœud, un seul `LEADER=true` ; `No resources found`
pour le PDB ; après la suppression, l'autre devient `true` en moins de ~20 s ; le rollout se
termine sans pod bloqué en `ContainerCreating` (`Multi-Attach`).

### C2. Supervision (MON-01 à MON-05)

```sh
curl -s http://127.0.0.1:8081/metrics | grep -E "^mimic_build_info|^mimic_controller_pods\{workload=\"otel-demo/shipping\"|^mimic_controller_certificate_expiry"
pod=$(kubectl -n "$NS" get pod -l app.kubernetes.io/name=shipping -o name | head -1)
kubectl -n "$NS" port-forward "$pod" 15100 &
curl -s http://127.0.0.1:15100/metrics | grep -E "^mimic_build_info|^mimic_store_disk"
kill %1
kubectl -n "$NS" get "$pod" -o jsonpath='{.spec.initContainers[?(@.name=="mimic")].env[?(@.name=="MIMIC_STORE_SIZE_LIMIT_BYTES")].value}'; echo
```

À voir :
- contrôleur : `mimic_build_info{mimic_version="0.1.0",revision=…} 1`, trois lignes
  `mimic_controller_pods{workload="otel-demo/shipping",state=…}` (`aligned` à 2), une date
  d'expiration de certificat d'environ 90 jours dans le futur (`date -d @<valeur>`) ;
- sidecar : `mimic_build_info`, `mimic_store_disk_bytes` non nul, `mimic_store_disk_limit_bytes
  536870912` ;
- variable : `536870912`.

Optionnel, avec un Prometheus opérateur : `kubectl apply -f deploy/monitoring/prometheus-rule.yaml`
→ `prometheusrule.monitoring.coreos.com/mimic created`.

### C3. Lecture par namespace : API agrégée (CTL-47, CTL-48, SEC-08, jamais exécuté en vrai)

```sh
kubectl get apiservice v1alpha1.data.mimic.platform.io
kubectl get --raw /apis/data.mimic.platform.io/v1alpha1 | jq -r .groupVersion
kubectl get --raw /apis/data.mimic.platform.io/v1alpha1/namespaces/$NS/workloads/shipping/operations | jq length
kubectl -n "$NS" create serviceaccount lecteur
kubectl -n "$NS" create rolebinding lecteur-mimic --clusterrole=mimic-data-viewer --serviceaccount="$NS:lecteur"
kubectl get --raw /apis/data.mimic.platform.io/v1alpha1/namespaces/$NS/workloads/shipping/operations \
  --as=system:serviceaccount:$NS:lecteur | jq length
kubectl get --raw /apis/data.mimic.platform.io/v1alpha1/namespaces/mimic-system/workloads/x/operations \
  --as=system:serviceaccount:$NS:lecteur
```

À voir : `AVAILABLE True` ; `data.mimic.platform.io/v1alpha1` ; un nombre d'opérations
(identique les deux fois) ; la dernière commande refusée en `403` avec un message qui nomme
`system:serviceaccount:otel-demo:lecteur` et `mimic-data-viewer`. Puis `mimicctl export
shipping -n $NS --inbound | head -3` marche sans port-forward (il passe par l'API agrégée).

### C4. mimicctl : existence du workload et appels sortants (CLI-18, CLI-19)

```sh
mimicctl mode shiping mock -n "$NS"; echo "code $?"
mimicctl requests shipping -n "$NS" --direction outbound --limit 3 | jq -r '.[].path'
mimicctl requests shipping -n "$NS" --direction sideways; echo "code $?"
```

À voir : refus, code 2, message qui liste les workloads injectés (dont `shipping`) et cite
`--force` ; trois fois `/getquote` ; refus de clap pour `sideways` (code 2), valeurs
possibles `inbound, outbound`.

### C5. Redaction (RED-08 à RED-11, CTL-49)

```sh
kubectl -n mimic-system exec deploy/mimic-controller -- ls /var/lib/mimic-controller/$NS/shipping/
curl -s http://127.0.0.1:8081/v1/workloads/$NS/shipping/requests?direction=outbound\&limit=50 | grep -c "redacted:sha256" || true
pod=$(kubectl -n "$NS" get pod -l app.kubernetes.io/name=shipping -o name | head -1)
kubectl -n "$NS" port-forward "$pod" 15100 &
curl -s http://127.0.0.1:15100/api/status | grep -c redact_salt; kill %1
```

À voir : un fichier `redact-salt` dans le répertoire du workload ; des marqueurs
`<redacted:sha256:…>` si la démo envoie des headers sensibles (sinon 0, ce n'est pas une
erreur) ; `0` : le sel n'apparaît jamais dans le statut du sidecar.

### C6. Mock gRPC avec trailers (MCK-28, sur h2c)

`frontend` appelle `shipping` en gRPC : le mettre en mock et arrêter `shipping` vérifie que
les réponses gRPC sont rejouées avec leurs trailers.

```sh
devis 20
mimicctl mode frontend mock -n "$NS"
sleep 10; mimicctl status frontend -n "$NS"
kubectl -n "$NS" scale deploy/shipping --replicas=0
devis 20
kubectl -n "$NS" scale deploy/shipping --replicas=2
mimicctl mode frontend record -n "$NS"
```

À voir : `ALIGNED` complet pour `frontend` ; avec `shipping` arrêté, `20/20 devis OK` (une
réponse gRPC sans `grpc-status` ferait échouer chaque devis).

### C7. Arrêt d'urgence (CTL-51, INJ-22, CLI-20)

```sh
mimicctl mode shipping mock -n "$NS"; sleep 10
mimicctl emergency-stop -n "$NS"
sleep 5
curl -s http://127.0.0.1:8081/v1/workloads | jq -c '.[] | select(.name == "shipping") | [.pods[].mode]'
sleep 10; kubectl -n "$NS" get mimicworkload shipping -o jsonpath='{.status.conditions[?(@.type=="Allowed")]}'; echo
kubectl -n "$NS" run sonde --image=busybox --restart=Never --dry-run=server -o jsonpath='{.spec.initContainers[*].name}'; echo
mimicctl emergency-stop -n "$NS" --off
sleep 5
curl -s http://127.0.0.1:8081/v1/workloads | jq -c '.[] | select(.name == "shipping") | [.pods[].mode]'
mimicctl mode shipping record -n "$NS"
```

À voir : le message dit `passthrough` et donne la commande `kubectl -n otel-demo rollout
restart` ; `["passthrough","passthrough"]` ; `Allowed` à `False`, raison `EmergencyStop` ;
le pod de sonde n'a **aucun** init container ; après `--off`, `["mock","mock"]` (même
snapshot, rien reconstruit).

### C8. Injection par pod (INJ-22)

```sh
kubectl create namespace volontaires
kubectl label namespace volontaires mimic.platform.io/enabled=opt-in pod-security.kubernetes.io/enforce=privileged
kubectl -n volontaires run sans --image=busybox --labels=app=sans --restart=Never --dry-run=server -o jsonpath='{.spec.initContainers[*].name}'; echo
kubectl -n volontaires run avec --image=busybox --labels=app=avec,mimic.platform.io/inject=true --restart=Never --dry-run=server -o jsonpath='{.spec.initContainers[*].name}'; echo
kubectl -n volontaires get configmap mimic-controller-ca
```

À voir : rien pour `sans`, `mimic-init mimic` pour `avec`, et la ConfigMap de la CA présente
(trust-manager suit aussi `opt-in`).

### C9. Variante CNI (CNI-01 à CNI-03, INJ-23, jamais exécutée)

```sh
manifest deploy/cni/mimic-cni.yaml | kubectl apply -f -
kubectl -n mimic-system rollout status ds/mimic-cni --timeout=120s
docker exec "$CLUSTER-control-plane" sh -c 'ls /opt/cni/bin/mimic-cni; cat /etc/cni/net.d/*.conflist' | grep -E "mimic-cni|\"type\""
# Même configuration que la démo, plus `cni: true`.
injecteur() {
  manifest deploy/injector/injector.yaml \
    | sed 's#requests: {cpu: 50m, memory: 64Mi}#requests: {cpu: 10m, memory: 64Mi}#' \
    | sed 's#^      mock:$#      redact:\n        raw_bodies: ["*", inbound]\n      mock:#' \
    | sed "$1" | kubectl apply -f -
  kubectl -n mimic-system rollout restart deploy/mimic-injector
}
injecteur 's/^    # cni: true$/    cni: true/'
kubectl -n mimic-system get configmap -o yaml | grep -c "^    cni: true"
kubectl -n mimic-system rollout status deploy/mimic-injector
kubectl create namespace sansprivilege
kubectl label namespace sansprivilege mimic.platform.io/enabled=true pod-security.kubernetes.io/enforce=baseline
kubectl -n sansprivilege create deployment web --image=nginx:1.27 --port=80
kubectl -n sansprivilege rollout status deploy/web --timeout=120s
kubectl -n sansprivilege get pod -l app=web -o jsonpath='{.items[0].spec.initContainers[*].name}{"\n"}{.items[0].metadata.annotations.mimic\.platform\.io/cni-rules}{"\n"}'
kubectl -n sansprivilege run c --image=curlimages/curl:8.11.1 --restart=Never --rm -i -- curl -s -o /dev/null -w "%{http_code}\n" http://web.sansprivilege
kubectl -n sansprivilege port-forward deploy/web 15100 & sleep 2
curl -s http://127.0.0.1:15100/metrics | grep 'mimic_requests_total{direction="inbound"'; kill %1
```

À voir :
- `1` (le webhook a bien `cni: true`) ;
- le binaire sur le nœud, et `"type": "mimic-cni"` en **dernier** dans la liste de plugins ;
- le pod `web` **admis en `baseline`** (sans la variante, l'API l'aurait refusé), avec pour
  seul init container `mimic`, et l'annotation `{"MIMIC_INBOUND_PORTS":"80",…}` ;
- `200`, puis une ligne `mimic_requests_total{direction="inbound",…} 1` : le trafic entrant
  est bien redirigé vers le sidecar sans `mimic-init`.

Remettre ensuite le webhook sans CNI, puis retirer le DaemonSet :

```sh
injecteur 's/^x$/x/'
kubectl delete -f deploy/cni/mimic-cni.yaml
sleep 15; docker exec "$CLUSTER-control-plane" sh -c 'cat /etc/cni/net.d/*.conflist' | grep -c mimic-cni
```

À voir : `0` : le plugin s'est retiré de la configuration du nœud à son arrêt.

### C10. Du cluster au poste (CLI-21, jamais exécuté contre un vrai contrôleur)

```sh
mimicctl mode shipping mock -n "$NS"; sleep 15
mimicctl pull shipping -n "$NS"
mimic-proxy --mock-from shipping-*.jsonl &
http_proxy=http://127.0.0.1:15101 curl -s -X POST http://quote:8080/getquote \
  -H 'content-type: application/json' -d '{"numberOfItems":3}' -i | head -5
kill %1; mimicctl mode shipping record -n "$NS"
```

À voir : `snapshot <id> (… échanges) écrit dans shipping-<id>.jsonl` ; puis une réponse
`200` avec `x-mimic-match` (le body exact dépend du trafic enregistré ; un `599` signifie
que ce body précis n'a jamais été vu, essayer avec ceux de `mimicctl requests shipping
--direction outbound`).

### C11. Logs du sidecar réglables (MON-03)

```sh
kubectl -n "$NS" patch deploy shipping -p '{"spec":{"template":{"metadata":{"annotations":{"mimic.platform.io/log":"debug"}}}}}'
kubectl -n "$NS" rollout status deploy/shipping
kubectl -n "$NS" get pod -l app.kubernetes.io/name=shipping -o jsonpath='{.items[0].spec.initContainers[?(@.name=="mimic")].env[?(@.name=="MIMIC_LOG")].value}'; echo
kubectl -n "$NS" run sonde --image=busybox --restart=Never --labels=app.kubernetes.io/name=sonde \
  --annotations=mimic.platform.io/log=mimic_proxy=bavard --dry-run=server -o name
```

À voir : `debug` ; puis `Warning: mimic.platform.io/log : mimic_proxy=bavard : …, valeur
ignorée` avant `pod/sonde` (les avertissements d'admission ne s'affichent qu'à qui crée le
pod, d'où le `kubectl run`).

---

## D. Nettoyage

```sh
kind delete cluster --name "$CLUSTER"
rm -f shipping-*.jsonl rapport.md /tmp/rwo.yaml /tmp/s3.yaml
```

## Résultat attendu

| Étape | Critères | OK / KO / remarque |
|-------|----------|--------------------|
| A1 | build, lint, tests | |
| A2 | MON-07 | |
| A3 | CTL-50 | |
| A4 | PRX-26 | |
| A5 | DOC-09 | |
| C1 | CTL-50, E2E-06 | |
| C2 | MON-01 à MON-05 | |
| C3 | CTL-47, CTL-48, SEC-08 | |
| C4 | CLI-18, CLI-19 | |
| C5 | RED-08 à RED-11, CTL-49 | |
| C6 | MCK-28 (h2c) | |
| C7 | CTL-51, INJ-22, CLI-20 | |
| C8 | INJ-22 | |
| C9 | CNI-01 à CNI-03, INJ-23 | |
| C10 | CLI-21 | |
| C11 | MON-03 | |
