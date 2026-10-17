#!/usr/bin/env sh
# Test d'interception transparente dans un namespace réseau jetable.
# Exige root (ou sudo) ; l'iptables de l'hôte n'est jamais modifié.
set -eu
cd "$(dirname "$0")/.."
cargo build --workspace --bins
cargo test -p mimic-proxy --test transparent --no-run
exec unshare --net -- sh -c '
  python3 scripts/lo-up.py
  MIMIC_NETNS_E2E=1 cargo test --offline -p mimic-proxy --test transparent -- --nocapture
'
