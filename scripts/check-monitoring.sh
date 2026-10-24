#!/usr/bin/env sh
# Vérifie les règles d'alerte livrées (spec §9.6, MON-07) : syntaxe, puis tests unitaires.
# Exige promtool (archive Prometheus) dans le PATH.
set -eu
cd "$(dirname "$0")/.."
promtool check rules deploy/monitoring/rules.yaml
promtool test rules deploy/monitoring/tests.yaml
