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
#    bascule du contrôleur (E2E-06) : la réplique active est supprimée sous trafic, l'autre
#    prend le relais en moins de FAILOVER_SECONDS, le mock et la découverte restent ;
# 7. retour en record ;
# 8. garde-fou production (E2E-04) : namespace labellisé env=production, mock refusé par le
#    contrôleur, par mimicctl et par la politique d'admission ;
# 9. WebSocket en mock (E2E-07, tests/e2e/websocket.yaml) : avec mock.websocket relay, la
#    poignée de main sortante d'un pod injecté aboutit (101) ; avec refuse, sous une
#    NetworkPolicy qui coupe sa sortie, elle reçoit le 503 du sidecar et le serveur ne voit
#    aucune connexion.
#
# Variables : CLUSTER (mimic), MIMIC_IMAGE (mimic:e2e), SKIP_BUILD=1 si l'image est déjà
# construite, MIMICCTL (chemin du binaire, sinon construit par cargo), LOAD_SECONDS (300),
# MOCK_WINDOW (60, secondes d'observation en mock global), REPORT_DIR (target/e2e-report),
# FAILOVER_SECONDS (30, délai de bascule du contrôleur toléré, spec §5.5.4).
set -euo pipefail
cd "$(dirname "$0")/.."

CLUSTER=${CLUSTER:-mimic}
IMAGE=${MIMIC_IMAGE:-mimic:e2e}
NS=otel-demo
LOAD_SECONDS=${LOAD_SECONDS:-300}
MOCK_WINDOW=${MOCK_WINDOW:-60}
# Bascule du contrôleur : bail rendu, relu sous 2 s, rechargement, endpoints, battement sous
# 2 s, soit moins de 10 s ; 30 s couvrent aussi un bail qui expire (15 s + 2 s) (§5.5.4).
FAILOVER_SECONDS=${FAILOVER_SECONDS:-30}
REPORT_DIR=${REPORT_DIR:-target/e2e-report}

# shellcheck source=scripts/e2e-lib.sh
. scripts/e2e-lib.sh

diagnostics() {
  log "diagnostic"
  kubectl get pods -A -o wide || true
  kubectl describe node | sed -n '/Allocated resources/,/Events/p' || true
  kubectl get mimicworkloads -A -o yaml || true
  kubectl -n mimic-system get lease mimic-controller -o yaml || true
  kubectl -n mimic-system get pods -l app.kubernetes.io/name=mimic-controller -o wide --show-labels || true
  kubectl -n mimic-system get endpointslices -l kubernetes.io/service-name=mimic-controller -o wide || true
  kubectl -n mimic-system logs -l app.kubernetes.io/name=mimic-controller --prefix --tail=100 || true
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
  # Étape WebSocket (E2E-07), si elle a commencé.
  if kubectl -n "$NS" get deploy ws-client >/dev/null 2>&1; then
    kubectl -n "$NS" logs deploy/ws-client -c mimic --tail=30 || true
    kubectl -n "$NS" logs deploy/ws-echo --tail=20 || true
    kubectl -n "$NS" get networkpolicy -o wide || true
  fi
  # Ce qui a été découvert jusque-là aide autant que les logs.
  [ -n "${reported:-}" ] || report || true
}
trap 'status=$?; [ $status -ne 0 ] && diagnostics; exit $status' EXIT

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

kind_cluster
install_mimic "$NS"

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
    kubectl get --raw "/api/v1/namespaces/$NS/pods/${p#pod/}:15100/proxy/metrics"
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
  grep -E '^mimic_(requests|upstream_errors|mock_responses|passthrough_connections)_total[{]' "$mdir/$w.after" || true
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

log "bascule du contrôleur sous trafic (E2E-06)"
# ctl_get <pod> <chemin> : GET sur l'interface d'un pod du contrôleur, liée à 127.0.0.1:8081.
ctl_get() {
  local port=$((20000 + RANDOM % 10000)) pid rc=1 i
  kubectl -n mimic-system port-forward "pod/$1" "$port:8081" >/dev/null 2>&1 &
  pid=$!
  for i in $(seq 1 20); do
    if curl -sf -m 20 "http://127.0.0.1:$port$2"; then rc=0; break; fi
    sleep 0.5
  done
  kill "$pid" 2>/dev/null || true
  wait "$pid" 2>/dev/null || true
  return $rc
}
# metric_of <nom> : valeur de la série, lue sur l'entrée standard (0 si absente) ; un nom
# sans labels retient aussi la série qui ne porte que des labels constants (`version`).
metric_of() { awk -v m="$1" '$1 == m || index($1, m "{") == 1 { v = $2 } END { printf "%d", v }'; }
pod_ip() { kubectl -n mimic-system get pod "$1" -o jsonpath='{.status.podIP}'; }
# Adresses prêtes du Service du contrôleur.
service_ips() {
  kubectl -n mimic-system get endpointslices -l kubernetes.io/service-name=mimic-controller \
    -o jsonpath='{.items[*].endpoints[?(@.conditions.ready==true)].addresses[*]}'
}
# État de shipping vu par une réplique : « alignés/sélectionnés snapshot ».
shipping_seen_by() {
  ctl_get "$1" /v1/workloads | jq -r --arg ns "$NS" \
    '.[] | select(.namespace == $ns and .name == "shipping") | "\(.aligned)/\(.selected_pods) \(.desired.snapshot)"'
}
# Pour chaque pod de shipping : taille du snapshot servi et nombre de téléchargements.
shipping_mock_state() {
  local p
  for p in $(kubectl -n "$NS" get pods -l app.kubernetes.io/name=shipping -o name | sort); do
    printf '%s %s %s\n' "${p#pod/}" \
      "$(kubectl get --raw "/api/v1/namespaces/$NS/pods/${p#pod/}:15100/proxy/metrics" | metric_of mimic_mock_snapshot_exchanges)" \
      "$(kubectl -n "$NS" logs "$p" -c mimic | grep -c 'snapshot chargé' || true)"
  done
}
EXEMPLARS=/var/lib/mimic-controller/$NS/shipping/exemplars.jsonl
old=$(leader)
[ "$(service_ips)" = "$(pod_ip "$old")" ] || fail "le Service ne route pas vers la seule réplique active ($(service_ips))"
snap=$(mimicctl status shipping -n "$NS" | tail -1 | awk '{ print $4 }')
[ -n "$snap" ] && [ "$snap" != "-" ] || fail "shipping n'a pas de snapshot"
# Exemples de shipping sur le PVC, écrits à chaque passe de maintenance (une minute).
exemplar_count() { kubectl -n mimic-system exec "$1" -- cat "$EXEMPLARS" 2>/dev/null | wc -l; }
has_exemplars() { [ "$(exemplar_count "$old")" -gt 0 ]; }
retry 90 has_exemplars || fail "aucun exemple de shipping sur le PVC"
exemplars_before=$(exemplar_count "$old")
digest_before=$(ctl_get "$old" "/v1/workloads/$NS/shipping/snapshots/$snap" | sha256sum)
mock_before=$(shipping_mock_state)
echo "active $old, snapshot $snap, $exemplars_before exemples ; sidecars : $(echo $mock_before)"
# Trafic continu pendant la bascule : quote est arrêté, shipping répond depuis son moteur.
fdir=$REPORT_DIR/failover
mkdir -p "$fdir"
rm -f "$fdir/stop"
( while [ ! -f "$fdir/stop" ]; do traffic 5 || echo "LOT EN ÉCHEC"; done ) > "$fdir/traffic.log" 2>&1 &
traffic_pid=$!
sleep 5
started=$SECONDS
kubectl -n mimic-system delete pod "$old" --wait=false
# Relais : une autre réplique porte la marque, seul endpoint du Service, et les deux pods de
# shipping s'y alignent de nouveau sur le même snapshot (battements repris).
new="" seen=""
until [ $((SECONDS - started)) -gt "$FAILOVER_SECONDS" ]; do
  new=$(leader)
  if [ "$(echo "$new" | wc -w)" -eq 1 ] && [ "$new" != "$old" ] &&
    [ "$(service_ips)" = "$(pod_ip "$new")" ]; then
    seen=$(shipping_seen_by "$new" || true)
    [ "$seen" = "2/2 $snap" ] && break
  fi
  sleep 1
done
failover=$((SECONDS - started))
[ "$seen" = "2/2 $snap" ] ||
  fail "pas de relais en $FAILOVER_SECONDS s : active « $new », endpoints « $(service_ips) », shipping « $seen »"
echo "relais par $new en ${failover} s"
[ "$(kubectl -n mimic-system get lease mimic-controller -o jsonpath='{.spec.holderIdentity}')" = "$new" ] ||
  fail "le bail n'est pas tenu par $new"
digest_after=$(ctl_get "$new" "/v1/workloads/$NS/shipping/snapshots/$snap" | sha256sum)
[ "$digest_after" = "$digest_before" ] || fail "snapshot $snap servi autrement par la nouvelle active"
mock_after=$(shipping_mock_state)
[ "$mock_after" = "$mock_before" ] ||
  fail "les sidecars ont rechargé leur snapshot : avant « $(echo $mock_before) », après « $(echo $mock_after) »"
retry 30 export_has '"/oteldemo.ShippingService/GetQuote"' shipping --inbound ||
  fail "découverte de shipping perdue à la bascule"
exemplars_after=$(exemplar_count "$new")
[ "$exemplars_after" -ge "$exemplars_before" ] || fail "exemples perdus : $exemplars_before, puis $exemplars_after"
[ "$(ctl_get "$new" /metrics | metric_of mimic_controller_exemplars)" -gt 0 ] ||
  fail "la nouvelle active n'a pas rechargé les exemples"
# La publication des sidecars reprend après son attente (10 à 40 s après une coupure brève).
segments_ok() { [ "$(ctl_get "$new" /metrics | metric_of 'mimic_controller_segments_total{result="ok"}')" -gt 0 ]; }
retry 90 segments_ok || fail "la nouvelle active ne reçoit pas de segments"
touch "$fdir/stop"
wait "$traffic_pid" || true
if grep -q "LOT EN ÉCHEC" "$fdir/traffic.log"; then
  cat "$fdir/traffic.log"
  fail "requêtes en échec pendant la bascule du contrôleur"
fi
echo "trafic pendant la bascule : $(grep -c '^5/5$' "$fdir/traffic.log") lots de 5 requêtes, tous réussis"
kubectl -n mimic-system rollout status deploy/mimic-controller --timeout=180s
passive=$(kubectl -n mimic-system get pod -l "$CTL_PODS,!mimic.platform.io/leader" -o jsonpath='{.items[*].metadata.name}')
[ "$(echo "$passive" | wc -w)" -eq 1 ] || fail "réplique passive attendue, trouvé « $passive »"
passive_metrics=$(ctl_get "$passive" /metrics) || fail "métriques de la passive"
echo "$passive_metrics" | grep -qx 'mimic_controller_leader 0' || fail "$passive ne se dit pas passive"
takeover=$(ctl_get "$new" /metrics | awk '$1 == "mimic_controller_takeover_seconds" { print $2 }')
{
  echo
  echo "## Bascule du contrôleur (E2E-06)"
  echo
  echo "Réplique active $old supprimée sous trafic : $new active et suivie par les deux pods de shipping en ${failover} s (borne $FAILOVER_SECONDS s), prise de relais ${takeover} s ; snapshot $snap servi à l'identique, $exemplars_after exemples, aucune requête en échec."
} | tee -a "$REPORT_DIR/mock.md" >> "${GITHUB_STEP_SUMMARY:-/dev/null}"

log "retour en record"
kubectl -n "$NS" scale deploy/quote --replicas=1
kubectl -n "$NS" rollout status deploy/quote --timeout=120s
mimicctl mode shipping record -n "$NS"
retry 60 sh -c "\"$MIMICCTL\" status shipping -n $NS | tail -1 | grep -qE 'record .* 2/2$'" || fail "retour en record"
traffic 10 || fail "requêtes en échec après le retour en record"

log "garde-fou production (E2E-04)"
PROD=mimic-prod
kubectl create namespace "$PROD" --dry-run=client -o yaml | kubectl apply -f -
kubectl label namespace "$PROD" env- >/dev/null 2>&1 || true
# Ressource en mock créée avant le label : le contrôleur la refuse dès que le label est posé.
echo '{"apiVersion":"mimic.platform.io/v1alpha1","kind":"MimicWorkload","metadata":{"name":"guard"},"spec":{"mode":"mock"}}' |
  kubectl -n "$PROD" apply -f -
kubectl label namespace "$PROD" env=production --overwrite
refused() {
  kubectl -n "$PROD" get mimicworkload guard \
    -o jsonpath='{.status.conditions[?(@.type=="Allowed")].reason}' | grep -qx ProductionNamespace
}
retry 60 refused || fail "condition Allowed=False absente"
code=0; mimicctl mode guard mock -n "$PROD" 2>/dev/null || code=$?
[ "$code" -eq 2 ] || fail "mimicctl mode mock accepté en production (code $code)"
# Politique d'admission : une ressource en mock y est refusée.
denied() {
  ! kubectl -n "$PROD" patch mimicworkload guard --type merge -p '{"spec":{"mode":"mock-partial"}}' 2>/dev/null
}
retry 30 denied || fail "politique d'admission inactive"
kubectl delete namespace "$PROD" --wait=false

log "WebSocket en mock : relais puis refus (E2E-07)"
# ws-echo (non injecté) compte les connexions et les poignées de main qu'il reçoit ; ws-client
# (curl, injecté) l'appelle à travers son sidecar.
kubectl apply -f tests/e2e/websocket.yaml
kubectl -n "$NS" rollout status deploy/ws-echo --timeout=120s
kubectl -n "$NS" rollout status deploy/ws-client --timeout=180s
ws_pod=$(kubectl -n "$NS" get pods -l app.kubernetes.io/name=ws-client \
  --field-selector=status.phase=Running -o jsonpath='{.items[0].metadata.name}')
case " $(kubectl -n "$NS" get pod "$ws_pod" -o jsonpath='{.spec.initContainers[*].name}') " in
  *" mimic "*) ;; *) fail "ws-client n'est pas injecté" ;;
esac
# ws_handshake : en-tête de la réponse à une poignée de main sortante de ws-client vers
# ws-echo. curl garde la connexion ouverte après un 101 : -m la coupe (code 28).
ws_handshake() {
  kubectl -n "$NS" exec "$ws_pod" -c curl -- curl -sS -i -N --http1.1 -m 3 \
    -H 'Connection: Upgrade' -H 'Upgrade: websocket' -H 'Sec-WebSocket-Version: 13' \
    -H 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==' http://ws-echo:8080/ws 2>/dev/null |
    tr -d '\r' | sed '/^$/q' || true
}
# ws_count : « connexions poignées » vues par ws-echo, lues depuis le pod client (non injecté).
ws_count() { kubectl -n "$NS" exec client -- curl -sS -m 5 http://ws-echo:8080/count; }
# ws_metric <result> : mimic_websocket_connections_total sortant du sidecar de ws-client.
ws_metric() {
  kubectl get --raw "/api/v1/namespaces/$NS/pods/$ws_pod:15100/proxy/metrics" |
    total '/^mimic_websocket_connections_total[{]/ && /direction="outbound"/ && /result="'"$1"'"/'
}
# ws_applied <websocket> : le sidecar de ws-client est en mock, snapshot chargé, avec ce réglage.
# Statut lu depuis le pod : hors sondes et /metrics, l'admin ne répond qu'à la boucle locale
# (spec §9.2), pas au proxy de l'API Kubernetes.
ws_applied() {
  kubectl -n "$NS" exec "$ws_pod" -c curl -- curl -sS -m 5 http://127.0.0.1:15100/api/status |
    jq -e --arg w "$1" '.mode == "mock" and .snapshot != null and .mock_websocket == $w' >/dev/null
}
# ws_aligned : statut publié pour la génération courante, le pod aligné.
ws_aligned() { mimicctl status ws-client -n "$NS" | tail -1 | grep -qE '\s1/1$'; }

# relay (défaut) : la poignée de main sortante joint ws-echo et aboutit.
mimicctl mode ws-client mock -n "$NS" >/dev/null
retry 90 ws_applied relay || fail "ws-client ne passe pas en mock"
retry 60 ws_aligned || { mimicctl status ws-client -n "$NS"; fail "ws-client non aligné en mock"; }
before=$(ws_count) relayed=$(ws_metric relayed)
head=$(ws_handshake)
echo "relay : $(echo "$head" | head -1)"
echo "$head" | head -1 | grep -q '^HTTP/1.1 101' || fail "relay : poignée de main non aboutie : $head"
after=$(ws_count)
[ "${after#* }" -gt "${before#* }" ] || fail "relay : ws-echo n'a pas reçu la poignée de main ($before -> $after)"
[ "$(ws_metric relayed)" -gt "$relayed" ] || fail "relay : result=\"relayed\" n'augmente pas"

# refuse : sortie de ws-client coupée sauf DNS et contrôleur ; le sidecar répond lui-même.
kubectl apply -f tests/e2e/websocket-egress.yaml
mimicctl mode ws-client mock --mock-websocket refuse -n "$NS" >/dev/null
retry 60 ws_applied refuse || fail "ws-client n'applique pas mock.websocket: refuse"
retry 60 ws_aligned || { mimicctl status ws-client -n "$NS"; fail "ws-client non aligné avec refuse"; }
before=$(ws_count) refused=$(ws_metric refused)
head=$(ws_handshake)
echo "refuse : $(echo "$head" | head -1)"
echo "$head" | head -1 | grep -q '^HTTP/1.1 503' || fail "refuse : 503 attendu : $head"
echo "$head" | grep -qix 'x-mimic-websocket: refused' || fail "refuse : x-mimic-websocket absent : $head"
after=$(ws_count)
[ "$after" = "$before" ] || fail "refuse : ws-echo a reçu une connexion ($before -> $after)"
[ "$(ws_metric refused)" -gt "$refused" ] || fail "refuse : result=\"refused\" n'augmente pas"
if [ -n "${GITHUB_STEP_SUMMARY:-}" ]; then
  printf '\n## WebSocket en mock (E2E-07)\n\n| mock.websocket | Réponse | ws-echo (connexions poignées) |\n|---|---|---|\n| relay | 101 | jointe |\n| refuse | 503, x-mimic-websocket: refused | %s, inchangé |\n' \
    "$after" >> "$GITHUB_STEP_SUMMARY"
fi
kubectl delete -f tests/e2e/websocket-egress.yaml -f tests/e2e/websocket.yaml --wait=false
kubectl -n "$NS" delete mimicworkload ws-client --wait=false

log "démo réussie"
