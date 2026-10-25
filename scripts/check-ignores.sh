#!/usr/bin/env bash
# Vérifie que chaque ignore de sécurité porte une raison et une échéance non dépassée :
#   - .trivyignore.yaml : `statement` et `expired_at` (AAAA-MM-JJ) par entrée ;
#   - commentaires `# zizmor: ignore[...]` : une date AAAA-MM-JJ de revue dans la ligne ;
#   - pas de zizmor.yml avec une liste `ignore` (non datable).
# Trivy ignore déjà de lui-même une entrée échue (le constat réapparaît) ; ce script
# donne en plus un message clair et refuse les entrées sans date.
set -euo pipefail
cd "$(dirname "$0")/.."
today=$(date -u +%F)
fail=0

if [ -f .trivyignore.yaml ]; then
  python3 -I - "$today" <<'PY' || fail=1
import re, sys
today = sys.argv[1]
entries, cur = [], None
for n, line in enumerate(open(".trivyignore.yaml"), 1):
    if re.match(r"\s*#", line) or not line.strip():
        continue
    m = re.match(r"\s*- id:\s*(\S+)", line)
    if m:
        cur = {"id": m.group(1), "line": n}
        entries.append(cur)
    elif cur is not None:
        m = re.match(r"\s*(statement|expired_at):\s*(.+?)\s*$", line)
        if m:
            cur[m.group(1)] = m.group(2).strip('"')
bad = 0
for e in entries:
    where = f".trivyignore.yaml:{e['line']} ({e['id']})"
    if not e.get("statement"):
        print(f"{where}: raison (statement) manquante")
        bad = 1
    d = e.get("expired_at", "")
    if not re.fullmatch(r"\d{4}-\d{2}-\d{2}", d):
        print(f"{where}: échéance (expired_at AAAA-MM-JJ) manquante")
        bad = 1
    elif d < today:
        print(f"{where}: échue le {d}, à revoir ou retirer")
        bad = 1
sys.exit(bad)
PY
fi

while IFS= read -r hit; do
  date=$(printf '%s' "$hit" | grep -oE '[0-9]{4}-[0-9]{2}-[0-9]{2}' | tail -n1 || true)
  if [ -z "$date" ]; then
    echo "${hit%%:*}: ignore zizmor sans date de revue (AAAA-MM-JJ)"
    fail=1
  elif [[ "$date" < "$today" ]]; then
    echo "${hit%%:*}: ignore zizmor échu le $date"
    fail=1
  fi
done < <(grep -rnE '#\s*zizmor:\s*ignore' .github || true)

for f in zizmor.yml zizmor.yaml .github/zizmor.yml .github/zizmor.yaml; do
  if [ -f "$f" ] && grep -qE '^\s*ignore:' "$f"; then
    echo "$f: liste ignore non datable, utiliser des commentaires inline datés"
    fail=1
  fi
done
exit "$fail"
