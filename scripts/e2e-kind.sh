#!/usr/bin/env bash
# Démo de bout en bout sur kind (spec §11, E2E) avec deux services de la démo
# OpenTelemetry : shipping appelle quote en HTTP.
#
# 1. installe cert-manager, trust-manager, le contrôleur et le webhook d'injection ;
# 2. déploie la démo : shipping reçoit mimic par le webhook ;
# 3. record : le trafic est relayé, l'API de shipping et celle de quote sont découvertes ;
# 4. mock (mimicctl) : quote est arrêté, shipping répond toujours depuis le snapshot ;
# 5. retour en record.
#
# Variables : CLUSTER (mimic), MIMIC_IMAGE (mimic:e2e), SKIP_BUILD=1 si l'image est déjà
# construite, MIMICCTL (chemin du binaire, sinon construit par cargo).
set -euo pipefail
cd "$(dirname "$0")/.."

CLUSTER=${CLUSTER:-mimic}
IMAGE=${MIMIC_IMAGE:-mimic:e2e}
NS=otel-demo
CERT_MANAGER=v1.21.1
TRUST_MANAGER=v0.24.0

log() { printf '\n== %s\n' "$*"; }
fail() { printf '\nÉCHEC : %s\n' "$*" >&2; exit 1; }

diagnostics() {
  log "diagnostic"
  kubectl get pods -A -o wide || true
  kubectl get mimicworkloads -A -o yaml || true
  kubectl -n mimic-system logs deploy/mimic-controller --tail=100 || true
  kubectl -n mimic-system logs deploy/mimic-injector --tail=50 || true
  for p in $(kubectl -n "$NS" get pods -l app.kubernetes.io/name=shipping -o name 2>/dev/null); do
    kubectl -n "$NS" describe "$p" | tail -30 || true
    kubectl -n "$NS" logs "$p" -c mimic --tail=80 || true
    kubectl -n "$NS" logs "$p" -c mimic-init || true
    kubectl -n "$NS" logs "$p" -c shipping --tail=30 || true
  done
}
trap 'status=$?; [ $status -ne 0 ] && diagnostics; exit $status' EXIT

# Réessaie une commande jusqu'à ce qu'elle réussisse, pendant $1 secondes.
retry() {
  local deadline=$((SECONDS + $1)); shift
  until "$@"; do
    [ $SECONDS -ge $deadline ] && return 1
    sleep 3
  done
}

if [ -z "${MIMICCTL:-}" ]; then
  cargo build -q -p mimicctl
  MIMICCTL=target/debug/mimicctl
fi
mimicctl() { "$MIMICCTL" "$@"; }

log "cluster kind $CLUSTER"
kind get clusters | grep -qx "$CLUSTER" || kind create cluster --name "$CLUSTER" --config tests/e2e/kind.yaml
if [ -z "${SKIP_BUILD:-}" ]; then
  docker build -t "$IMAGE" .
fi
kind load docker-image "$IMAGE" --name "$CLUSTER"

log "cert-manager et trust-manager"
kubectl apply -f "https://github.com/cert-manager/cert-manager/releases/download/$CERT_MANAGER/cert-manager.yaml" >/dev/null
kubectl -n cert-manager wait --for=condition=Available deploy --all --timeout=180s
# La CA du contrôleur vit dans mimic-system : c'est le namespace de confiance de trust-manager.
kubectl apply -f deploy/controller/namespace.yaml
helm repo add jetstack https://charts.jetstack.io --force-update >/dev/null
helm upgrade --install trust-manager jetstack/trust-manager \
  --version "$TRUST_MANAGER" -n cert-manager --set app.trust.namespace=mimic-system --wait

# Les manifests livrés, avec l'image construite pour le test.
manifest() { sed "s#registry.example.com/platform/mimic:0.1.0#$IMAGE#g" "$@"; }

log "contrôleur"
kubectl apply -f deploy/controller/crd.yaml -f deploy/controller/rbac.yaml
retry 60 kubectl apply -f deploy/controller/certificates.yaml
manifest deploy/controller/controller.yaml | kubectl apply -f -
kubectl -n mimic-system rollout status deploy/mimic-controller --timeout=180s

log "webhook d'injection"
manifest deploy/injector/injector.yaml | kubectl apply -f -
kubectl -n mimic-system rollout status deploy/mimic-injector --timeout=180s
# Le caBundle est posé par le cainjector : attendre qu'il le soit avant de créer des pods.
retry 60 sh -c '[ -n "$(kubectl get mutatingwebhookconfiguration mimic-injector -o jsonpath="{.webhooks[0].clientConfig.caBundle}")" ]'

log "démo OpenTelemetry"
kubectl apply -f tests/e2e/otel-demo.yaml
retry 60 kubectl -n "$NS" get configmap mimic-controller-ca >/dev/null
kubectl -n "$NS" rollout status deploy/quote --timeout=180s
kubectl -n "$NS" rollout status deploy/shipping --timeout=240s
kubectl -n "$NS" wait --for=condition=Ready pod/client --timeout=120s

log "injection"
for p in $(kubectl -n "$NS" get pods -l app.kubernetes.io/name=shipping -o name); do
  inits=$(kubectl -n "$NS" get "$p" -o jsonpath='{.spec.initContainers[*].name}')
  [ "$inits" = "mimic-init mimic" ] || fail "$p : init containers « $inits »"
done
quote_inits=$(kubectl -n "$NS" get pods -l app.kubernetes.io/name=quote -o jsonpath='{.items[0].spec.initContainers}')
[ -z "$quote_inits" ] || fail "quote injecté malgré mimic.platform.io/inject=false"

# N requêtes POST /get-quote depuis le client ; échoue si l'une n'est pas un 200 chiffré.
traffic() {
  kubectl -n "$NS" exec client -- sh -c '
    ok=0
    for i in $(seq 1 '"$1"'); do
      body=$(curl -sS -m 5 -X POST http://shipping:50050/get-quote \
        -H "content-type: application/json" \
        -d "{\"items\":[{\"quantity\":3}],\"address\":{\"zip_code\":\"94043\"}}" -w "\n%{http_code}") || continue
      code=$(echo "$body" | tail -1)
      echo "$body" | head -1 | grep -q cost_usd && [ "$code" = 200 ] && ok=$((ok + 1))
    done
    echo "$ok/'"$1"'"
    [ "$ok" -eq '"$1"' ]'
}

log "record : trafic relayé et enregistré"
traffic 40 || fail "requêtes en échec en record"

log "découverte centralisée"
export_has() { mimicctl export shipping -n "$NS" "${@:2}" 2>/dev/null | grep -q "$1"; }
retry 90 export_has '"/get-quote"' --inbound || fail "API de shipping absente du contrôleur"
retry 30 export_has '"/getquote"' --outbound quote:8090 || fail "API de quote absente du contrôleur"
mimicctl export shipping -n "$NS" --inbound --openapi-version 3.0 -o /tmp/shipping-3.0.yaml
grep -q "openapi: 3.0.3" /tmp/shipping-3.0.yaml || fail "export 3.0"
mimicctl export shipping -n "$NS" --inbound -o /tmp/shipping.json
mimicctl diff /tmp/shipping.json /tmp/shipping.json --fail-on-breaking
mimicctl requests shipping -n "$NS" --format k6 | grep -q "k6/http" || fail "script k6"

log "bascule en mock"
mimicctl mode shipping mock -n "$NS"
aligned() {
  local line
  line=$(mimicctl status shipping -n "$NS" | tail -1)
  echo "$line"
  echo "$line" | grep -qE '\s2/2$' && ! echo "$line" | grep -qE '\s-\s'
}
retry 90 aligned || fail "les pods ne s'alignent pas sur le snapshot"

log "mock : quote arrêté, shipping répond depuis le snapshot"
kubectl -n "$NS" scale deploy/quote --replicas=0
kubectl -n "$NS" wait --for=delete pod -l app.kubernetes.io/name=quote --timeout=60s
traffic 40 || fail "requêtes en échec en mock"

log "retour en record"
kubectl -n "$NS" scale deploy/quote --replicas=1
kubectl -n "$NS" rollout status deploy/quote --timeout=120s
mimicctl mode shipping record -n "$NS"
retry 60 sh -c "\"$MIMICCTL\" status shipping -n $NS | tail -1 | grep -qE 'record .* 2/2$'" || fail "retour en record"
traffic 10 || fail "requêtes en échec après le retour en record"

log "démo réussie"
