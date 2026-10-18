#!/usr/bin/env bash
# Mimic dans un service mesh, sur kind (spec §3.3, E2E-08 et E2E-09).
#
#   scripts/e2e-mesh.sh istio     profil ambient d'Istio : un namespace en sidecar
#                                 (istio-injection=enabled) et un en ambient
#                                 (istio.io/dataplane-mode=ambient), PeerAuthentication STRICT ;
#   scripts/e2e-mesh.sh linkerd   un namespace injecté par Linkerd, politique entrante
#                                 all-authenticated.
#
# Dans chaque namespace, un serveur HTTP et un client curl (tests/e2e/mesh.yaml), injectés
# par le mesh puis par mimic :
# 1. mimic passe après les conteneurs du mesh et mimic-init reconnaît le mesh ;
# 2. le client appelle le serveur par son Service : 200 ; un pod hors mesh est refusé par le
#    serveur, preuve que le trafic du client passe par le mTLS du mesh ;
# 3. les sidecars mimic du client (sortant) et du serveur (entrant) ont enregistré l'échange ;
# 4. client en mock, serveur arrêté : le client reçoit la même réponse, servie par son sidecar.
#
# Variables : CLUSTER (mimic), MIMIC_IMAGE (mimic:e2e), SKIP_BUILD=1 si l'image est déjà
# construite, MIMICCTL (chemin du binaire, sinon construit par cargo), ISTIO_VERSION,
# LINKERD_VERSION, BIN (target/e2e-bin, où sont téléchargés istioctl et linkerd).
set -euo pipefail
cd "$(dirname "$0")/.."

MESH=${1:-}
CLUSTER=${CLUSTER:-mimic}
IMAGE=${MIMIC_IMAGE:-mimic:e2e}
ISTIO_VERSION=${ISTIO_VERSION:-1.30.6}
LINKERD_VERSION=${LINKERD_VERSION:-edge-26.10.1}
BIN=${BIN:-$PWD/target/e2e-bin}
OUTSIDE=mesh-outside

# shellcheck source=scripts/e2e-lib.sh
. scripts/e2e-lib.sh

case "$MESH" in
  istio) cases="sidecar ambient" ;;
  linkerd) cases="linkerd" ;;
  *) fail "usage : $0 istio|linkerd" ;;
esac

ns_of() { echo "mesh-$1"; }

diagnostics() {
  log "diagnostic"
  kubectl get pods -A -o wide || true
  kubectl describe node | sed -n '/Allocated resources/,/Events/p' || true
  kubectl get mimicworkloads -A || true
  kubectl -n mimic-system logs -l app.kubernetes.io/name=mimic-controller --prefix --tail=60 || true
  kubectl -n mimic-system logs deploy/mimic-injector --tail=30 || true
  local c ns p
  for c in $cases; do
    ns=$(ns_of "$c")
    kubectl get namespace "$ns" >/dev/null 2>&1 || continue
    mimicctl status -n "$ns" || true
    kubectl -n "$ns" get events --sort-by=.lastTimestamp | tail -30 || true
    for p in $(kubectl -n "$ns" get pods -o name); do
      echo "-- $ns/$p"
      kubectl -n "$ns" get "$p" -o jsonpath='init: {.spec.initContainers[*].name}{"\n"}conteneurs: {.spec.containers[*].name}{"\n"}annotations: {.metadata.annotations}{"\n"}'
      kubectl -n "$ns" describe "$p" | tail -25 || true
      kubectl -n "$ns" logs "$p" -c mimic-init 2>/dev/null || true
      kubectl -n "$ns" logs "$p" -c mimic --tail=40 2>/dev/null || true
      kubectl -n "$ns" logs "$p" -c istio-proxy --tail=30 2>/dev/null || true
      kubectl -n "$ns" logs "$p" -c linkerd-proxy --tail=30 2>/dev/null || true
    done
  done
  if [ "$MESH" = istio ]; then
    kubectl -n istio-system logs ds/ztunnel --tail=60 || true
    kubectl -n istio-system logs ds/istio-cni-node --tail=40 || true
  fi
}
trap 'status=$?; [ $status -ne 0 ] && diagnostics; exit $status' EXIT

install_istio() {
  log "Istio $ISTIO_VERSION (profil ambient)"
  mkdir -p "$BIN"
  [ -x "$BIN/istioctl" ] ||
    curl -sSfL "https://github.com/istio/istio/releases/download/$ISTIO_VERSION/istioctl-$ISTIO_VERSION-linux-amd64.tar.gz" |
    tar xz -C "$BIN"
  # Un seul nœud : requêtes CPU réduites pour que les pods du test y tiennent.
  "$BIN/istioctl" install -y --set profile=ambient \
    --set values.pilot.resources.requests.cpu=100m \
    --set values.global.proxy.resources.requests.cpu=10m
  kubectl -n istio-system rollout status deploy/istiod --timeout=300s
  kubectl -n istio-system rollout status ds/istio-cni-node --timeout=300s
  kubectl -n istio-system rollout status ds/ztunnel --timeout=300s
}

install_linkerd() {
  log "Linkerd $LINKERD_VERSION"
  local linkerd=$HOME/.linkerd2/bin/linkerd
  [ -x "$linkerd" ] && "$linkerd" version --client --short 2>/dev/null | grep -qx "$LINKERD_VERSION" ||
    curl --proto '=https' --tlsv1.2 -sSfL https://run.linkerd.io/install-edge | LINKERD2_VERSION=$LINKERD_VERSION sh
  "$linkerd" install --crds --set installGatewayAPI=true | kubectl apply -f -
  "$linkerd" install | kubectl apply -f -
  "$linkerd" check --wait 5m
}

# Namespace du cas $1, labellisé pour mimic et pour le mesh, avec le mTLS exigé.
setup_ns() {
  local ns
  ns=$(ns_of "$1")
  kubectl create namespace "$ns" --dry-run=client -o yaml | kubectl apply -f -
  kubectl label namespace "$ns" mimic.platform.io/enabled=true --overwrite
  case "$1" in
    sidecar) kubectl label namespace "$ns" istio-injection=enabled --overwrite ;;
    ambient) kubectl label namespace "$ns" istio.io/dataplane-mode=ambient --overwrite ;;
    linkerd)
      kubectl annotate namespace "$ns" linkerd.io/inject=enabled \
        config.linkerd.io/default-inbound-policy=all-authenticated --overwrite ;;
  esac
  if [ "$MESH" = istio ]; then
    kubectl apply -f - <<EOF
apiVersion: security.istio.io/v1
kind: PeerAuthentication
metadata: {name: default, namespace: $ns}
spec: {mtls: {mode: STRICT}}
EOF
  fi
}

pod_of() { kubectl -n "$1" get pod -l "app.kubernetes.io/name=$2" -o name | head -1; }

# Ordre des conteneurs et mesh vu par mimic-init, pour chaque pod du cas $1.
check_injection() {
  local ns app p inits containers mesh_name expected
  ns=$(ns_of "$1")
  case "$1" in
    sidecar) mesh_name=istio expected="mesh IstioSidecar" ;;
    ambient) mesh_name="" expected="mesh IstioAmbient" ;;
    linkerd) mesh_name=linkerd expected="mesh Linkerd" ;;
  esac
  for app in mesh-server mesh-client; do
    p=$(pod_of "$ns" "$app")
    inits=$(kubectl -n "$ns" get "$p" -o jsonpath='{.spec.initContainers[*].name}')
    containers=$(kubectl -n "$ns" get "$p" -o jsonpath='{.spec.containers[*].name}')
    echo "$p : init « $inits », conteneurs « $containers »"
    case " $inits " in *" mimic-init mimic "*) ;; *) fail "$p : mimic absent des init containers" ;; esac
    if [ -n "$mesh_name" ]; then
      case " $inits $containers " in *" $mesh_name-proxy "*) ;; *) fail "$p : pas de $mesh_name-proxy" ;; esac
      # Aucun conteneur du mesh après mimic-init dans les init containers.
      echo "$inits" | tr ' ' '\n' | sed -n '/^mimic-init$/,$p' | grep -q "^$mesh_name-" &&
        fail "$p : un conteneur du mesh passe après mimic-init"
    else
      [ "$(kubectl -n "$ns" get "$p" -o jsonpath='{.metadata.annotations.ambient\.istio\.io/redirection}')" = enabled ] ||
        fail "$p : pas pris en charge par ztunnel"
    fi
    kubectl -n "$ns" logs "$p" -c mimic-init
    kubectl -n "$ns" logs "$p" -c mimic-init | grep -q "$expected" || fail "$p : mimic-init n'a pas reconnu le mesh ($expected)"
  done
}

# traffic <namespace> <n> : n appels du client au serveur, tous 200 avec le corps attendu.
traffic() {
  kubectl -n "$1" exec deploy/mesh-client -c curl -- sh -c '
    ok=0
    for i in $(seq 1 '"$2"'); do
      out=$(curl -sS -m 5 -w " %{http_code}" http://mesh-server:8080/ping 2>&1) || true
      if [ "$out" = "pong
 200" ] || [ "$out" = "pong 200" ]; then ok=$((ok + 1)); else echo "échec $i : $out"; fi
    done
    echo "$ok/'"$2"'"
    [ "$ok" -eq '"$2"' ]'
}

# Le serveur refuse un client hors mesh (mTLS exigé) : tout sauf un 200.
refused_outside() {
  local code
  code=$(kubectl -n "$OUTSIDE" exec outsider -- curl -s -m 5 -o /dev/null -w '%{http_code}' \
    "http://mesh-server.$1:8080/ping" 2>/dev/null) || true
  echo "client hors mesh : ${code:-000}"
  [ "${code:-000}" != 200 ]
}

export_has() { mimicctl export "$2" -n "$3" "${@:4}" 2>/dev/null | grep -q "$1"; }

aligned() {
  local line
  line=$(mimicctl status "$2" -n "$1" | tail -1)
  echo "$line"
  echo "$line" | grep -qE '\s1/1$' && ! echo "$line" | grep -qE '\s-\s'
}

kind_cluster
case "$MESH" in
  istio) install_istio ;;
  linkerd) install_linkerd ;;
esac
first=${cases%% *}
install_mimic "$(ns_of "$first")"

log "client hors mesh"
kubectl create namespace "$OUTSIDE" --dry-run=client -o yaml | kubectl apply -f -
kubectl -n "$OUTSIDE" run outsider --image=curlimages/curl:8.11.1 --restart=Never \
  --dry-run=client -o yaml --command -- sleep infinity | kubectl apply -f -
kubectl -n "$OUTSIDE" wait --for=condition=Ready pod/outsider --timeout=120s

for c in $cases; do
  ns=$(ns_of "$c")
  log "$c : déploiement dans $ns"
  setup_ns "$c"
  kubectl -n "$ns" apply -f tests/e2e/mesh.yaml
  kubectl -n "$ns" rollout status deploy/mesh-server --timeout=300s
  kubectl -n "$ns" rollout status deploy/mesh-client --timeout=300s

  log "$c : injection"
  check_injection "$c"

  log "$c : trafic par le mesh"
  retry 60 traffic "$ns" 1 >/dev/null || traffic "$ns" 1 || fail "$c : le client n'atteint pas le serveur"
  traffic "$ns" 20 || fail "$c : appels en échec"
  refused_outside "$ns" || fail "$c : le serveur accepte un client hors mesh, mTLS non exigé"

  log "$c : enregistrement par mimic"
  retry 90 export_has '"/ping"' mesh-client "$ns" --outbound mesh-server:8080 ||
    fail "$c : appel sortant du client non enregistré"
  retry 90 export_has '"/ping"' mesh-server "$ns" --inbound ||
    fail "$c : appel entrant du serveur non enregistré"

  log "$c : client en mock, serveur arrêté"
  mimicctl mode mesh-client mock -n "$ns"
  retry 90 aligned "$ns" mesh-client || fail "$c : le client ne s'aligne pas sur un snapshot"
  kubectl -n "$ns" scale deploy/mesh-server --replicas=0
  kubectl -n "$ns" wait --for=delete pod -l app.kubernetes.io/name=mesh-server --timeout=120s
  traffic "$ns" 20 || fail "$c : réponses du mock en échec"

  # Place pour le cas suivant sur l'unique nœud.
  [ "$c" = "${cases##* }" ] || kubectl delete namespace "$ns" --wait=false
done

log "mimic dans le mesh $MESH : OK"
