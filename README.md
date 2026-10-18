# mimic

Sidecar Kubernetes qui observe le trafic HTTP d'un pod pour en déduire l'API (OpenAPI 3.1),
puis rejoue les réponses des dépendances en mode mock pour lancer des tirs de performance
sans toucher aux services réels.

La spécification complète, les critères d'acceptation et le plan de livraison sont dans
[docs/SPEC.md](docs/SPEC.md).

## État

| Phase | Contenu | État |
|-------|---------|------|
| P0 | Workspace, CI, modèle `Exchange`, fixtures | fait |
| P1 | Inférence OpenAPI et redaction (`mimic-core`) | fait |
| P2 | Moteur de mock (`mimic-core::mock`) | fait |
| P3 | Proxy `mimic-proxy` : record, mock, page d'admin, métriques | fait |
| P3b | Partage entre sidecars d'un workload | fait |
| P4 | `mimic-init` (iptables), détection de protocole, image, manifests | fait |
| P5 | Interception TLS sortante | fait |
| P6 | Contrôleur, CRD `MimicWorkload`, snapshots, authentification, rejeu k6 | fait (non validé sur un vrai cluster) |
| P7 | Webhook d'injection `mimic-injector` et CLI `mimicctl` | fait, validé sur kind avec la démo OpenTelemetry |
| P6d | Parcours SRE : garde-fou production, `capture.inbound`, `selector`, label `version` des métriques | fait (étape kind E2E-04 ajoutée, non jouée en local) |

## Lancer le sidecar en local

```sh
cargo run -p mimic-proxy -- --config exemple.yaml
```

```yaml
mode: record
inbound:
  listen: 127.0.0.1:15006
  upstream: 127.0.0.1:8080   # l'application
outbound:
  listen: 127.0.0.1:15001    # à utiliser comme HTTP_PROXY par l'application
admin:
  listen: 127.0.0.1:15000    # page d'admin sur http://127.0.0.1:15000/
```

Bascule de mode : `curl -X PUT --data mock http://127.0.0.1:15000/mode`.

Déploiement dans Kubernetes : voir [deploy/README.md](deploy/README.md).

## mimicctl

Référence complète, droits nécessaires et interface web du contrôleur : [docs/cli.md](docs/cli.md).
Tir de charge d'un workload isolé, métriques à suivre et exemple blue/green avec Argo Rollouts :
[docs/sre-load-test.md](docs/sre-load-test.md).

```sh
cargo install --path crates/mimicctl
mimicctl mode checkout mock -n shop          # bascule tous les pods du workload
mimicctl status -A                           # mode souhaité, snapshot, pods alignés
mimicctl export checkout -n shop --inbound --openapi-version 3.0 -o checkout.yaml
mimicctl requests checkout -n shop --format k6 > replay.js
mimicctl diff semaine-1.yaml semaine-2.yaml --fail-on-breaking
mimicctl report -n shop > decouverte.md      # API, top 10 des appels et leurs headers (Markdown)
```

## Développement

```sh
cargo test --workspace          # tests unitaires, propriétés et snapshots
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo insta review              # après un changement voulu de la sortie OpenAPI
sudo ./scripts/e2e-netns.sh     # interception iptables réelle, dans un namespace réseau jetable
./scripts/e2e-kind.sh           # démo OpenTelemetry sur kind (charge comprise) : injection, rapport, bascule mock
```

Reproduire et lire le test sur kind : [docs/e2e-kind.md](docs/e2e-kind.md).

Les tests portent l'identifiant du critère d'acceptation qu'ils couvrent
(`inf_01_…`, `red_04_…`), voir la section 11 de la spec.
