#!/usr/bin/env bash
# Fonctions communes aux tests de bout en bout sur kind (scripts/e2e-kind.sh,
# scripts/e2e-mesh.sh) : cluster, installation de mimic, attentes.
#
# Variables lues : CLUSTER, IMAGE, MIMICCTL (chemin du binaire, sinon construit par cargo),
# SKIP_BUILD=1 si l'image est déjà construite.

CERT_MANAGER=v1.21.1
TRUST_MANAGER=v0.24.0

log() { printf '\n== %s\n' "$*"; }
fail() { printf '\nÉCHEC : %s\n' "$*" >&2; exit 1; }

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

# cert-manager peut réémettre la CA juste après sa création : un certificat feuille
# signé entre-temps par l'ancienne CA reste « Ready » mais n'est plus vérifiable par les
# sidecars (invalid peer certificate: BadSignature). On compare la CA qui a signé la
# feuille à la CA publiée et, si elles diffèrent, on supprime le Secret de la feuille
# pour que cert-manager la réémette avec la CA courante.
leaf_signed_by_current_ca() {
  local ca leaf
  ca=$(kubectl -n mimic-system get secret mimic-controller-ca -o jsonpath='{.data.tls\.crt}')
  leaf=$(kubectl -n mimic-system get secret "$1" -o jsonpath='{.data.ca\.crt}')
  [ -n "$ca" ] && [ "$ca" = "$leaf" ]
}
reissue_if_stale() {
  local secret attempt
  for secret in "$@"; do
    for attempt in 1 2 3; do
      kubectl -n mimic-system wait --for=condition=Ready certificate --all --timeout=120s >/dev/null
      leaf_signed_by_current_ca "$secret" && break
      echo "$secret signé par une ancienne CA : réémission ($attempt)"
      kubectl -n mimic-system delete secret "$secret"
      REISSUED=1
      sleep 3
    done
    leaf_signed_by_current_ca "$secret" || fail "$secret n'est pas signé par la CA publiée"
  done
}

# Les manifests livrés, avec l'image construite pour le test.
manifest() { sed "s#ghcr.io/thaliph/mimic:0.1.0#$IMAGE#g" "$@"; }

# Deux réplicas prêts, dont une seule active : celle qui porte la marque (spec §5.5.4).
CTL_PODS=app.kubernetes.io/name=mimic-controller
leader() {
  kubectl -n mimic-system get pod -l "$CTL_PODS,mimic.platform.io/leader=true" \
    -o jsonpath='{.items[*].metadata.name}'
}
has_leader() { [ "$(leader | wc -w)" -eq 1 ]; }

# Cluster kind, créé s'il n'existe pas, avec l'image de mimic chargée.
kind_cluster() {
  log "cluster kind $CLUSTER"
  kind get clusters | grep -qx "$CLUSTER" || kind create cluster --name "$CLUSTER" --config tests/e2e/kind.yaml
  if [ -z "${SKIP_BUILD:-}" ]; then
    docker build -t "$IMAGE" .
  fi
  kind load docker-image "$IMAGE" --name "$CLUSTER"
}

# cert-manager, trust-manager, contrôleur et webhook d'injection ; rend la main quand le
# webhook injecte les pods du namespace $1 (labellisé mimic.platform.io/enabled=true).
install_mimic() {
  local NS=$1
  log "cert-manager et trust-manager"
  kubectl apply -f "https://github.com/cert-manager/cert-manager/releases/download/$CERT_MANAGER/cert-manager.yaml" >/dev/null
  kubectl -n cert-manager wait --for=condition=Available deploy --all --timeout=180s
  # La CA du contrôleur vit dans mimic-system : c'est le namespace de confiance de trust-manager.
  kubectl apply -f deploy/controller/namespace.yaml
  helm repo add jetstack https://charts.jetstack.io --force-update >/dev/null
  helm upgrade --install trust-manager jetstack/trust-manager \
    --version "$TRUST_MANAGER" -n cert-manager --set app.trust.namespace=mimic-system --wait

  log "contrôleur"
  kubectl apply -f deploy/controller/crd.yaml -f deploy/controller/rbac.yaml \
    -f deploy/controller/production-guard.yaml
  retry 60 kubectl apply -f deploy/controller/certificates.yaml
  reissue_if_stale mimic-controller-tls
  # Même raison que pour les sidecars ci-dessous : sur l'unique nœud de kind, trois
  # réplicas (deux plus celle du rollout) demandant 1Gi chacune laissent trop peu de place.
  manifest deploy/controller/controller.yaml \
    | sed 's#requests: {cpu: 100m, memory: 1Gi}#requests: {cpu: 100m, memory: 256Mi}#' \
    | kubectl apply -f -
  kubectl -n mimic-system rollout status deploy/mimic-controller --timeout=180s
  retry 60 has_leader || fail "aucune réplique active du contrôleur"
  echo "réplique active : $(leader)"

  log "webhook d'injection"
  # kind n'a qu'un nœud : avec deux réplicas du contrôleur, les requêtes CPU des sidecars
  # (50m chacun, une vingtaine de pods) ne laissaient plus de place aux pods de test
  # ajoutés en fin de script (Insufficient cpu). Le test ne porte pas sur ces requêtes :
  # on les abaisse à 10m pour les sidecars injectés ici.
  manifest deploy/injector/injector.yaml \
    | sed 's#requests: {cpu: 50m, memory: 64Mi}#requests: {cpu: 10m, memory: 64Mi}#' \
    | kubectl apply -f -
  reissue_if_stale mimic-injector-tls
  # Le webhook relit son certificat toutes les 10 s ; relancé si sa feuille a été réémise,
  # pour ne pas attendre cette relecture.
  [ -z "${REISSUED:-}" ] || kubectl -n mimic-system rollout restart deploy/mimic-injector
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
}
