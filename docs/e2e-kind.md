# Reproduire le test de bout en bout sur kind

`scripts/e2e-kind.sh` déploie mimic et la démo OpenTelemetry dans un cluster kind, laisse
tourner le générateur de charge de la démo, publie le rapport de découverte, puis vérifie
les bascules en mock (critère E2E-05 de la [spec](SPEC.md)). La CI le lance sur chaque PR
vers `main` (job `e2e-kind`) ; la même commande marche sur un poste.

## Prérequis

| Outil | Version testée | Rôle |
|-------|----------------|------|
| Docker | | image `mimic:e2e` et nœud kind |
| kind | 0.27 | cluster local, nœud `kindest/node:v1.32.2` (native sidecars : Kubernetes 1.29+) |
| kubectl | | |
| helm | 3 | trust-manager |
| jq, openssl | | diagnostics en cas d'échec |
| cargo | stable | construit `mimicctl` si `MIMICCTL` n'est pas fourni |

Prévoir une dizaine de Go de disque libre : les images de la démo font à elles seules
plusieurs Go. Le premier passage télécharge tout ; les suivants réutilisent le cluster.

## Lancer

```sh
./scripts/e2e-kind.sh
```

Le script crée le cluster `mimic` s'il n'existe pas, construit l'image, l'y charge, puis
déroule le scénario. En CI, le scénario prend une dizaine de minutes une fois le cluster
créé ; en local, le premier passage dure surtout le temps de télécharger les images. Il s'arrête à la première erreur avec `ÉCHEC : …` et affiche un
diagnostic : pods, `MimicWorkload`, logs du contrôleur et du webhook, événements, empreintes
des CA, logs des sidecars de shipping et des pods non prêts, et le rapport s'il n'a pas
encore été produit.

Variables utiles :

| Variable | Défaut | Effet |
|----------|--------|-------|
| `CLUSTER` | `mimic` | nom du cluster kind |
| `MIMIC_IMAGE` | `mimic:e2e` | image du sidecar, du contrôleur et du webhook |
| `SKIP_BUILD=1` | | réutilise l'image déjà construite |
| `MIMICCTL` | construit par cargo | chemin d'un binaire `mimicctl` |
| `LOAD_SECONDS` | `300` | durée de récolte sous la charge du générateur |
| `MOCK_WINDOW` | `60` | durée d'observation du mock global, en secondes |
| `REPORT_DIR` | `target/e2e-report` | rapport, bilan du mock et documents OpenAPI |

Relancer sans reconstruire : `SKIP_BUILD=1 ./scripts/e2e-kind.sh`. La démo est réappliquée
à chaque passage ; pour repartir de zéro, `kind delete cluster --name mimic`.

## Ce que fait le scénario

1. **Socle** : cert-manager, trust-manager, contrôleur (`deploy/controller/`) et webhook
   d'injection (`deploy/injector/`).
2. **Démo** : namespace `otel-demo` labellisé `mimic.platform.io/enabled=true`. Le script
   attend que le webhook injecte réellement (pod sonde en `--dry-run=server`), applique
   `tests/e2e/otel-demo.yaml`, attend tous les déploiements, puis vérifie que chaque pod a
   `mimic-init` et le sidecar, sauf `valkey-cart` (opt-out) et le pod `client`.
3. **Record** : des devis sont demandés au frontend (`GET /api/shipping`), qui appelle
   shipping en gRPC, qui appelle quote en HTTP. Les API gRPC de shipping et HTTP de quote
   doivent apparaître dans le contrôleur ; export OpenAPI 3.0, `diff` et script k6 sont
   vérifiés au passage.
4. **Rapport** : après `LOAD_SECONDS` de charge, `mimicctl report -n otel-demo` écrit
   `REPORT_DIR/report.md` (API par workload, top 10 des appels et leurs headers) et les
   documents OpenAPI de chaque service dans `REPORT_DIR/openapi/`.
5. **Mock global** : tous les workloads passent en mock sauf le générateur, qui reste le
   client. Une fois les pods alignés, les métriques des sidecars sont relevées pendant
   `MOCK_WINDOW` secondes : aucune requête entrante ne doit arriver ailleurs que chez les
   services appelés par le générateur (frontend-proxy, et flagd pour ses feature flags), et
   frontend-proxy doit répondre depuis son snapshot. Bilan dans `REPORT_DIR/mock.md`, puis
   retour en record.
6. **Mock de shipping** : `mimicctl mode shipping mock`, quote est arrêté, et 40 devis
   doivent encore réussir ; retour en record, 10 devis de plus.

En CI, le rapport et le bilan du mock sont ajoutés au résumé du job et imprimés dans le log
(groupe « rapport de découverte »), et `REPORT_DIR` est publié en artefact `e2e-report`.

## Regarder pendant le test

```sh
kubectl -n otel-demo get pods -w
mimicctl status -n otel-demo                       # mode, snapshot, pods alignés
kubectl port-forward -n mimic-system deploy/mimic-controller 8081
# puis http://127.0.0.1:8081/ : workloads, API découvertes, snapshots, requêtes
kubectl port-forward -n otel-demo deploy/frontend 15000
# puis http://127.0.0.1:15000/ : page d'admin d'un sidecar, /metrics pour ses métriques
```

## Régénérer la démo

`tests/e2e/otel-demo.yaml` est généré depuis le manifest officiel de la démo :

```sh
python3 scripts/otel-demo.py > tests/e2e/otel-demo.yaml        # version par défaut (2.1.3)
python3 scripts/otel-demo.py 2.2.0 > tests/e2e/otel-demo.yaml  # autre tag de la démo
```

PyYAML est requis.

Le script retire la chaîne de télémétrie et Kafka, désactive l'injection de `valkey-cart`,
passe shipping à 2 réplicas et retire flagd-ui. Ne pas éditer le YAML généré à la main.
