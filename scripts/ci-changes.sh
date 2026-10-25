#!/usr/bin/env bash
# Calcule, pour ci.yml et security.yml, quels groupes de jobs doivent tourner (politique CI, #71).
# Écrit rust=, cluster=, netns=, e2e= dans $GITHUB_OUTPUT (et sur la sortie standard).
# Entrées (variables d'environnement, jamais interpolées dans le YAML) :
#   EVENT_NAME, BASE_REF, BASE_SHA, LABELS_JSON (liste JSON des étiquettes de la PR).
set -euo pipefail

event="${EVENT_NAME:-}"
base_ref="${BASE_REF:-}"
base_sha="${BASE_SHA:-}"
labels="${LABELS_JSON:-[]}"

rust=false
cluster=false
forced=false

if [ "$event" = "pull_request" ]; then
  files="$(git diff --name-only "$base_sha"...HEAD)"
  if printf '%s\n' "$labels" | grep -q '"e2e"'; then forced=true; fi
  while IFS= read -r f; do
    [ -n "$f" ] || continue
    case "$f" in
      crates/*|Cargo.toml|Cargo.lock|.cargo/*|rust-toolchain|rust-toolchain.*|deny.toml)
        rust=true ;;
    esac
    case "$f" in
      deploy/monitoring/*) ;;
      crates/*|Cargo.toml|Cargo.lock|.cargo/*|rust-toolchain|rust-toolchain.*|deploy/*|\
      Dockerfile|.dockerignore|scripts/e2e-*|scripts/otel-demo.py|tests/e2e/*|\
      .github/workflows/ci.yml|scripts/ci-changes.sh)
        cluster=true ;;
    esac
  done <<<"$files"
else
  # push sur main, nuit, déclenchement manuel : tout le code est concerné.
  rust=true
fi

# e2e-netns : hors PR toujours ; sur PR dès que le Rust ou le cluster change.
netns=false
if [ "$event" != "pull_request" ] || [ "$rust" = true ] || [ "$cluster" = true ] || [ "$forced" = true ]; then
  netns=true
fi

# kind et mesh : PR vers main filtrée (ou étiquette e2e), nuit et manuel ; pas sur push main.
e2e=false
case "$event" in
  schedule|workflow_dispatch) e2e=true ;;
  pull_request)
    if [ "$base_ref" = "main" ] && { [ "$cluster" = true ] || [ "$forced" = true ]; }; then e2e=true; fi ;;
esac

out="${GITHUB_OUTPUT:-/dev/stdout}"
{
  echo "rust=$rust"
  echo "cluster=$cluster"
  echo "netns=$netns"
  echo "e2e=$e2e"
} | tee -a "$out"
