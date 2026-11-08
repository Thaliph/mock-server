#!/usr/bin/env bash
# test-valkey.sh: make sure the local Valkey used by `cargo test` runs, with the image the chart
# pins (deploy/helm/mimic/values.yaml), on 127.0.0.1:16432 (6379 is taken on this machine).
# Prints the variable to export. Idempotent; survives reboots (--restart unless-stopped).
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"
name=${MIMIC_TEST_VALKEY_CONTAINER:-f8-5w}
port=${MIMIC_TEST_VALKEY_PORT:-16432}
values=deploy/helm/mimic/values.yaml
field() { sed -n '/^valkey:/,/^[a-z]/p' "$values" | awk -v k="$1:" '$1 == k { print $2; exit }'; }
image="$(field repository):$(field tag)@$(field digest)"
if [[ $(docker inspect -f '{{.State.Running}}' "$name" 2>/dev/null) != true ]]; then
  docker rm -f "$name" >/dev/null 2>&1 || true
  docker run -d --name "$name" --restart unless-stopped -p "127.0.0.1:$port:6379" "$image" >/dev/null
fi
for _ in $(seq 1 20); do
  [[ $(docker exec "$name" valkey-cli ping 2>/dev/null) == PONG ]] && break
  sleep 0.5
done
[[ $(docker exec "$name" valkey-cli ping 2>/dev/null) == PONG ]] || { echo "valkey $name not answering" >&2; exit 1; }
echo "export MIMIC_TEST_VALKEY_URL=redis://127.0.0.1:$port"
