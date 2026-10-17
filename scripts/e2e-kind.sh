#!/usr/bin/env bash
# Démo de bout en bout sur kind (spec §11, E2E-05) avec la démo OpenTelemetry et son
# générateur de charge (tests/e2e/otel-demo.yaml, généré par scripts/otel-demo.py).
#
# 1. installe cert-manager, trust-manager, le contrôleur et le webhook d'injection ;
# 2. déploie la démo : tous ses services reçoivent mimic par le webhook, sauf valkey-cart ;
# 3. record : devis demandés au frontend (gRPC vers shipping, HTTP vers quote) ; l'API de
#    shipping et celle de quote sont découvertes ;
# 4. après LOAD_SECONDS de charge, rapport de découverte (mimicctl report) et documents
#    OpenAPI dans REPORT_DIR, rapport ajouté au résumé du job GitHub Actions ;
# 5. mock global : tout passe en mock sauf le générateur ; les métriques des sidecars
#    montrent que seules ses cibles (frontend-proxy, flagd) reçoivent encore du trafic,
#    frontend-proxy répondant depuis le snapshot ;
# 6. mock de shipping (mimicctl) : quote est arrêté, shipping répond depuis le snapshot ;
# 7. retour en record.
#
# Variables : CLUSTER (mimic), MIMIC_IMAGE (mimic:e2e), SKIP_BUILD=1 si l'image est déjà
# construite, MIMICCTL (chemin du binaire, sinon construit par cargo), LOAD_SECONDS (300),
# MOCK_WINDOW (60, secondes d'observation en mock global), REPORT_DIR (target/e2e-report).
set -euo pipefail
cd "$(dirname "$0")/.."

CLUSTER=${CLUSTER:-mimic}
IMAGE=${MIMIC_IMAGE:-mimic:e2e}
NS=otel-demo
LOAD_SECONDS=${LOAD_SECONDS:-300}
MOCK_WINDOW=${MOCK_WINDOW:-60}
REPORT_DIR=${REPORT_DIR:-target/e2e-report}
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
  kubectl -n "$NS" get events --sort-by=.lastTimestamp | tail -40 || true
  # Confiance sidecars → contrôleur : la CA publiée doit avoir signé le certificat servi.
  kubectl -n mimic-system get certificates,certificaterequests -o wide || true
  for src in "secret/mimic-controller-ca ca\.crt" "secret/mimic-controller-tls ca\.crt" "configmap/mimic-controller-ca ca\.crt"; do
    set -- $src
    for ns in mimic-system "$NS"; do
      pem=$(kubectl -n "$ns" get "$1" -o jsonpath="{.data.$2}" 2>/dev/null) || continue
      [ -n "$pem" ] || continue
      case "$1" in secret/*) pem=$(echo "$pem" | base64 -d) ;; esac
      echo "$ns/$1 : $(echo "$pem" | openssl x509 -noout -fingerprint -sha256 -startdate 2>/dev/null | tr '\n' ' ')"
    done
  done
  df -h / 2>/dev/null | tail -1 || true
  docker exec "$CLUSTER-control-plane" df -h /var/lib/containerd 2>/dev/null | tail -1 || true
  # shipping, puis tout pod de la démo qui n'est pas prêt.
  local pods
  pods=$( (kubectl -n "$NS" get pods -l app.kubernetes.io/name=shipping -o name
    kubectl -n "$NS" get pods -o json 2>/dev/null | jq -r '.items[]
      | select(any(.status.conditions[]?; .type == "Ready" and .status != "True"))
      | "pod/" + .metadata.name') | sort -u) || true
  for p in $pods; do
    kubectl -n "$NS" describe "$p" | tail -30 || true
    # Premières erreurs, puis les plus récentes.
    kubectl -n "$NS" logs "$p" -c mimic 2>/dev/null | grep -m5 '"WARN"\|"ERROR"' || true
    kubectl -n "$NS" logs "$p" -c mimic --tail=40 || true
    kubectl -n "$NS" logs "$p" -c mimic-init || true
    kubectl -n "$NS" logs "$p" --all-containers --tail=30 || true
  done
  # Ce qui a été découvert jusque-là aide autant que les logs.
  [ -n "${reported:-}" ] || report || true
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

# Rapport de découverte et documents OpenAPI des services ; ajouté au résumé du job.
report() {
  mkdir -p "$REPORT_DIR/openapi"
  mimicctl report -n "$NS" > "$REPORT_DIR/report.md"
  for d in $(kubectl -n "$NS" get deploy -o name); do
    mimicctl export "${d#*/}" -n "$NS" --inbound -o "$REPORT_DIR/openapi/${d#*/}.yaml" 2>/dev/null || true
  done
  if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
    cat "$REPORT_DIR/report.md" >> "$GITHUB_STEP_SUMMARY"
    echo "::group::rapport de découverte"
    cat "$REPORT_DIR/report.md"
    echo "::endgroup::"
  fi
  reported=1
  echo "rapport : $REPORT_DIR/report.md"
}

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

# Le webhook est en failurePolicy Ignore : un pod créé avant que son Service réponde n'est
# pas injecté. Attendre qu'un pod d'essai (dry-run côté serveur) ressorte injecté.
kubectl create namespace "$NS" --dry-run=client -o yaml | kubectl apply -f -
kubectl label namespace "$NS" mimic.platform.io/enabled=true --overwrite
injects() {
  kubectl -n "$NS" run mimic-probe --image=busybox --restart=Never --dry-run=server \
    --labels app.kubernetes.io/name=mimic-probe -o jsonpath='{.spec.initContainers[*].name}' 2>/dev/null |
    grep -qw mimic
}
retry 120 injects || fail "le webhook n'injecte pas"

log "démo OpenTelemetry"
kubectl apply -f tests/e2e/otel-demo.yaml -f tests/e2e/client.yaml
retry 60 kubectl -n "$NS" get configmap mimic-controller-ca >/dev/null
for d in $(kubectl -n "$NS" get deploy -o name); do
  kubectl -n "$NS" rollout status "$d" --timeout=600s
done
kubectl -n "$NS" wait --for=condition=Ready pod/client --timeout=120s
load_started=$SECONDS

log "injection"
for p in $(kubectl -n "$NS" get pods -o name); do
  inits=$(kubectl -n "$NS" get "$p" -o jsonpath='{.spec.initContainers[*].name}')
  case "$p" in
    pod/client|pod/valkey-cart-*)
      case " $inits " in *" mimic "*) fail "$p injecté malgré mimic.platform.io/inject=false" ;; esac ;;
    *)
      case "$inits " in "mimic-init mimic "*) ;; *) fail "$p : init containers « $inits »" ;; esac ;;
  esac
done

# N devis de livraison demandés au frontend (GET /api/shipping) : frontend appelle shipping
# en gRPC, shipping appelle quote en HTTP. Échoue si l'un n'est pas un 200 chiffré.
traffic() {
  kubectl -n "$NS" exec client -- sh -c '
    ok=0
    for i in $(seq 1 '"$1"'); do
      body=$(curl -sS -m 10 -G http://frontend-proxy:8080/api/shipping \
        --data-urlencode "itemList=[{\"productId\":\"66VCHSJNUP\",\"quantity\":3}]" \
        --data-urlencode "currencyCode=USD" \
        --data-urlencode "address={\"streetAddress\":\"1600 Amphitheatre Parkway\",\"city\":\"Mountain View\",\"state\":\"CA\",\"country\":\"United States\",\"zipCode\":\"94043\"}" \
        -w "\n%{http_code}") || { echo "échec $i : curl"; continue; }
      code=$(echo "$body" | tail -1)
      if echo "$body" | head -1 | grep -q currencyCode && [ "$code" = 200 ]; then
        ok=$((ok + 1))
      elif [ $((i - ok)) -le 3 ]; then
        # Les premiers échecs disent quel service a cédé (message gRPC du frontend).
        echo "échec $i : $code $(echo "$body" | head -1 | cut -c1-300)"
      fi
    done
    echo "$ok/'"$1"'"
    [ "$ok" -eq '"$1"' ]'
}

log "record : trafic relayé et enregistré"
# Les services de la démo n'ont pas de readiness probe : attendre qu'ils répondent.
echo "attente de la démo (5 min au plus)"
retry 300 traffic 1 >/dev/null 2>&1 || fail "la démo ne répond pas"
traffic 40 || fail "requêtes en échec en record"

log "découverte centralisée"
# export_has <motif> <workload> <arguments de mimicctl export>
export_has() { mimicctl export "$2" -n "$NS" "${@:3}" 2>/dev/null | grep -q "$1"; }
retry 90 export_has '"/oteldemo.ShippingService/GetQuote"' shipping --inbound || fail "API de shipping absente du contrôleur"
retry 30 export_has '"/getquote"' shipping --outbound quote:8080 || fail "API de quote absente du contrôleur"
mimicctl export shipping -n "$NS" --inbound --openapi-version 3.0 -o /tmp/shipping-3.0.yaml
grep -q "openapi: 3.0.3" /tmp/shipping-3.0.yaml || fail "export 3.0"
mimicctl export shipping -n "$NS" --inbound -o /tmp/shipping.json
mimicctl diff /tmp/shipping.json /tmp/shipping.json --fail-on-breaking
mimicctl requests shipping -n "$NS" --format k6 | grep -q "k6/http" || fail "script k6"

log "charge du générateur de la démo ($LOAD_SECONDS s)"
remaining=$((load_started + LOAD_SECONDS - SECONDS))
[ $remaining -gt 0 ] && sleep "$remaining"
# Le trafic du générateur entre par frontend-proxy et descend jusqu'au catalogue.
retry 60 export_has '"/api/products' frontend-proxy --inbound || fail "trafic du générateur absent"
report

log "mock global : seules les cibles du générateur reçoivent du trafic"
# Le générateur reste en record : c'est le client. Tous les autres sidecars répondent
# depuis leur snapshot ; seuls les services qu'il appelle lui-même (frontend-proxy, et
# flagd pour ses feature flags) reçoivent encore des requêtes.
mocked=$(kubectl -n "$NS" get deploy -o name | sed 's|.*/||' | grep -vxE 'load-generator|valkey-cart')
for w in $mocked; do mimicctl mode "$w" mock -n "$NS" >/dev/null; done
# all_aligned <mode> : chaque workload de $mocked a tous ses pods sur <mode> (et un snapshot en mock).
all_aligned() {
  local out w
  out=$(mimicctl status -n "$NS")
  for w in $mocked; do
    echo "$out" | awk -v w="$w" -v m="$1" '
      $2 == w && $3 == m && (m != "mock" || $4 != "-") { split($5, a, "/"); if (a[2] > 0 && a[1] == a[2]) ok = 1 }
      END { exit !ok }' || return 1
  done
}
retry 120 all_aligned mock || { mimicctl status -n "$NS"; fail "les pods ne s'alignent pas en mock"; }
# Laisser finir les requêtes parties avant la bascule.
sleep 15
# scrape <workload> : métriques des sidecars de tous ses pods.
scrape() {
  local p
  for p in $(kubectl -n "$NS" get pods -l "opentelemetry.io/name=$1" -o name); do
    kubectl get --raw "/api/v1/namespaces/$NS/pods/${p#pod/}:15000/proxy/metrics"
  done
}
# total <filtre awk> : somme des séries retenues, lues sur l'entrée standard.
total() { awk "$1"' { s += $NF } END { printf "%d", s }'; }
# by_authority <filtre awk> : « authority somme » des séries retenues, triées.
by_authority() {
  awk "$1"' { match($0, /authority="[^"]*"/); s[substr($0, RSTART + 11, RLENGTH - 12)] += $NF }
    END { for (a in s) printf "%s %d\n", a, s[a] }' | LC_ALL=C sort
}
# delta <workload> <filtre awk> : évolution de la somme pendant la fenêtre.
delta() { echo $(( $(total "$2" < "$mdir/$1.after") - $(total "$2" < "$mdir/$1.before") )); }
INBOUND='/^mimic_requests_total[{]/ && /direction="inbound"/'
OUTBOUND='/^mimic_requests_total[{]/ && /direction="outbound"/'
SERVED='/^mimic_mock_responses_total[{]/ && !/match="unmatched"/'
UNMATCHED='/^mimic_mock_responses_total[{]/ && /match="unmatched"/'
mdir=$REPORT_DIR/mock
mkdir -p "$mdir"
for w in $mocked load-generator; do scrape "$w" > "$mdir/$w.before"; done
sleep "$MOCK_WINDOW"
for w in $mocked load-generator; do scrape "$w" > "$mdir/$w.after"; done
# Appels du générateur pendant la fenêtre, par authority.
LC_ALL=C join -a 2 -e 0 -o 0,1.2,2.2 \
  <(by_authority "$OUTBOUND" < "$mdir/load-generator.before") \
  <(by_authority "$OUTBOUND" < "$mdir/load-generator.after") \
  | awk '$3 > $2 { print $1, $3 - $2 }' > "$mdir/targets"
{
  echo "## Mock global ($MOCK_WINDOW s, tout sauf load-generator)"
  echo
  echo "| Workload | Appels du générateur | Requêtes entrantes | Réponses du snapshot | Sans correspondance |"
  echo "|---|---:|---:|---:|---:|"
} > "$REPORT_DIR/mock.md"
leaks="" front_calls=0 front_in=0 front_served=0
for w in $mocked; do
  calls=$(awk -v w="$w" '{ split($1, h, ":") } h[1] == w { s += $2 } END { printf "%d", s }' "$mdir/targets")
  inb=$(delta "$w" "$INBOUND")
  served=$(delta "$w" "$SERVED")
  unmatched=$(delta "$w" "$UNMATCHED")
  echo "| $w | $calls | $inb | $served | $unmatched |" >> "$REPORT_DIR/mock.md"
  [ "$calls" -gt 0 ] || [ "$inb" -eq 0 ] || leaks="$leaks $w($inb)"
  if [ "$w" = frontend-proxy ]; then front_calls=$calls front_in=$inb front_served=$served; fi
done
cat "$REPORT_DIR/mock.md"
[ -n "${GITHUB_STEP_SUMMARY:-}" ] && { echo; cat "$REPORT_DIR/mock.md"; } >> "$GITHUB_STEP_SUMMARY"
echo "::group::métriques de frontend-proxy et du générateur"
for w in frontend-proxy load-generator; do
  echo "-- $w"
  grep -E '^mimic_(requests|upstream_errors|mock_responses)_total[{]' "$mdir/$w.after" || true
  kubectl -n "$NS" logs "deploy/$w" -c mimic 2>/dev/null | grep '"WARN"\|"ERROR"' | tail -10 || true
done
echo "::endgroup::"
[ -z "$leaks" ] || fail "trafic entrant en mock hors des cibles du générateur :$leaks"
[ "$front_calls" -gt 0 ] && [ "$front_in" -gt 0 ] || fail "frontend-proxy ne reçoit plus le trafic du générateur"
[ "$front_served" -gt 0 ] || fail "frontend-proxy ne répond pas depuis le snapshot"

log "retour en record après le mock global"
for w in $mocked; do mimicctl mode "$w" record -n "$NS" >/dev/null; done
retry 120 all_aligned record || { mimicctl status -n "$NS"; fail "retour en record"; }

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
