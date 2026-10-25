#!/usr/bin/env bash
# Rend les variantes du contrôleur dans deploy/install/, que `mimicctl install` embarque
# (spec §9.5b). À relancer après toute modification de deploy/controller*/ ; CLI-22 vérifie
# que les rendus sont à jour quand kubectl est présent.
set -euo pipefail
cd "$(dirname "$0")/.."
render() {
  {
    echo "# Généré par scripts/render-install.sh depuis deploy/$1 : ne pas modifier à la main."
    kubectl kustomize "deploy/$1"
  } > "deploy/install/$2"
}
render controller controller.yaml
render controller-rwo controller-rwo.yaml
render controller-s3 controller-s3.yaml
