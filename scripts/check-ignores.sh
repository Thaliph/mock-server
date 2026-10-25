#!/usr/bin/env bash
# Vérifie que chaque ignore de sécurité porte une raison et une échéance maîtrisée :
#   - .trivyignore.yaml, lu comme du vrai YAML : chaque entrée de chaque section
#     (misconfigurations, vulnerabilities, secrets, licenses) exige `id`, `statement` non
#     vide et `expired_at` (AAAA-MM-JJ) entre aujourd'hui et +12 mois ; toute forme illisible
#     ou inconnue échoue ;
#   - commentaires `# zizmor: ignore[règle]` : une raison puis une date de revue AAAA-MM-JJ
#     (même fenêtre) ;
#   - aucun fichier de configuration zizmor non vide (il pourrait désactiver un audit) ;
#   - aucun trivy.yaml / trivy.yml non vide à la racine (configuration par défaut de Trivy,
#     que trivy-action ne remplace pas : skip-dirs/skip-files masqueraient des constats) ;
#   - les ignores zizmor sont cherchés dans tous les YAML du dépôt (actions composites
#     comprises), un seul ignore par ligne ;
#   - pas de .trivyignore texte (format sans raison ni date).
# Trivy ignore déjà de lui-même une entrée échue (le constat réapparaît) ; ce script
# donne en plus un message clair et refuse les entrées sans date.
# Dépend de python3 et de PyYAML (présents sur ubuntu-latest).
set -euo pipefail
cd "$(dirname "$0")/.."
exec python3 -I - "$(date -u +%F)" <<'PY'
import datetime, glob, os, re, sys

import yaml

today = datetime.date.fromisoformat(sys.argv[1])
limit = today + datetime.timedelta(days=366)
bad = False


def fail(where, msg):
    global bad
    print(f"{where}: {msg}")
    bad = True


def check_date(where, value):
    """Valide une date de revue ; `value` est une date YAML ou une chaîne."""
    if isinstance(value, datetime.datetime):
        value = None
    if isinstance(value, datetime.date):
        d = value
    elif isinstance(value, str) and re.fullmatch(r"\d{4}-\d{2}-\d{2}", value):
        try:
            d = datetime.date.fromisoformat(value)
        except ValueError:
            return fail(where, f"date invalide {value!r}")
    else:
        return fail(where, "échéance (AAAA-MM-JJ) manquante ou illisible")
    if d < today:
        fail(where, f"échue le {d}, à revoir ou retirer")
    elif d > limit:
        fail(where, f"échéance {d} trop lointaine (12 mois au plus)")


# 1. Trivy : configuration racine interdite, puis .trivyignore.yaml.
for f in sorted(glob.glob("trivy.y*ml")):
    try:
        content = yaml.safe_load(open(f, encoding="utf-8"))
    except yaml.YAMLError:
        content = "illisible"
    if content not in (None, {}):
        fail(f, "configuration Trivy non vide interdite (skip-dirs, skip-files, severity… contournent le contrôle) ; utiliser .trivyignore.yaml")
for legacy in (".trivyignore", ".trivyignore.yml"):
    if os.path.exists(legacy):
        fail(legacy, "sans raison ni date, utiliser .trivyignore.yaml")
if os.path.exists(".trivyignore.yaml"):
    f = ".trivyignore.yaml"
    doc = {}
    try:
        doc = yaml.safe_load(open(f, encoding="utf-8"))
    except yaml.YAMLError as e:
        fail(f, f"YAML illisible : {e}")
    if not isinstance(doc, dict):
        if doc is not None:
            fail(f, "doit être une table de sections")
        doc = {}
    sections = {"misconfigurations", "vulnerabilities", "secrets", "licenses"}
    for name, entries in doc.items():
        if name not in sections:
            fail(f, f"section inconnue {name!r}")
            continue
        if not isinstance(entries, list):
            fail(f"{f} [{name}]", "doit être une liste")
            continue
        for i, e in enumerate(entries, 1):
            where = f"{f} [{name}] entrée {i}"
            if not isinstance(e, dict):
                fail(where, "entrée illisible")
                continue
            where += f" ({e.get('id')})"
            if not isinstance(e.get("id"), str) or not e["id"].strip():
                fail(where, "id manquant")
            st = e.get("statement")
            if not isinstance(st, str) or not st.strip():
                fail(where, "raison (statement) manquante")
            check_date(where, e.get("expired_at"))

# 2. zizmor : configuration (interdite si non vide) et ignores inline.
seen = set()
for pat in ("zizmor.y*ml", ".github/zizmor.y*ml", ".github/**/zizmor.y*ml"):
    for f in sorted(set(glob.glob(pat, recursive=True)) - seen):
        seen.add(f)
        try:
            content = yaml.safe_load(open(f, encoding="utf-8"))
        except yaml.YAMLError:
            content = "illisible"
        if content not in (None, {}):
            fail(f, "configuration zizmor non vide interdite (désactiver un audit contourne le contrôle) ; utiliser des ignores inline")

inline = re.compile(r"#\s*zizmor:\s*ignore(.*)$")
for root, dirs, files in os.walk("."):
    dirs[:] = [d for d in dirs if d not in (".git", "target", "node_modules")]
    for name in files:
        path = os.path.join(root, name)[2:]
        if not (path.startswith(".github/") or name.endswith((".yml", ".yaml"))):
            continue
        try:
            lines = open(path, encoding="utf-8").read().splitlines()
        except (UnicodeDecodeError, OSError):
            continue
        for n, line in enumerate(lines, 1):
            m = inline.search(line)
            if not m:
                continue
            where = f"{path}:{n}"
            if len(re.findall(r"zizmor:\s*ignore", line)) > 1:
                fail(where, "un seul ignore zizmor par ligne")
                continue
            rule = re.match(r"\[[^\]]+\](.*)$", m.group(1))
            if not rule:
                fail(where, "ignore zizmor sans règle entre crochets")
                continue
            tail = rule.group(1)
            dates = re.findall(r"\d{4}-\d{2}-\d{2}", tail)
            if not dates:
                fail(where, "ignore zizmor sans date de revue (AAAA-MM-JJ)")
                continue
            check_date(where, dates[-1])
            reason = re.sub(r"\d{4}-\d{2}-\d{2}", "", tail)
            reason = re.sub(r"à revoir avant|revue|review|before|until", "", reason, flags=re.I)
            if len(re.sub(r"[\W_]+", "", reason)) < 10:
                fail(where, "ignore zizmor sans raison (texte avant la date)")

sys.exit(1 if bad else 0)
PY
