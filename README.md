# mimic

Sidecar Kubernetes qui observe le trafic HTTP d'un pod pour en déduire l'API (OpenAPI 3.1),
puis rejoue les réponses des dépendances en mode mock pour lancer des tirs de performance
sans toucher aux services réels.

La spécification complète, les critères d'acceptation et le plan de livraison sont dans
[docs/SPEC.md](docs/SPEC.md). Pour installer, configurer, superviser et exploiter mimic : le
[guide d'exploitation](docs/guide/README.md). Pour l'essayer sur un poste, pas à pas : la
[démo locale sur kind](docs/demo-kind.md).

## État

| Phase | Contenu | État |
|-------|---------|------|
| P0 | Workspace, CI, modèle `Exchange`, fixtures | fait |
| P1 | Inférence OpenAPI et redaction (`mimic-core`) | fait |
| P2 | Moteur de mock (`mimic-core::mock`) | fait |
| P3 | Proxy `mimic-proxy` : record, mock, page d'admin, métriques | fait |
| P3b | Partage entre sidecars d'un workload | fait |
| P3g | WebSocket en mock réglable : `mock.websocket: relay` (défaut) ou `refuse` (`503` sans joindre la dépendance), par la configuration, la `MimicWorkload` et `mimicctl mode --mock-websocket` | fait (étape kind E2E-07 ajoutée, jouée par la CI) |
| P4 | `mimic-init` (iptables), détection de protocole, image, manifests | fait |
| P4b | Service mesh : Istio sidecar, Istio ambient, Linkerd ; Mimic entre l'application et le mesh | fait, validé sur kind (E2E-08, E2E-09) |
| P5 | Interception TLS sortante | fait |
| P6 | Contrôleur, CRD `MimicWorkload`, snapshots, authentification, rejeu k6 | fait (non validé sur un vrai cluster) |
| P7 | Webhook d'injection `mimic-injector` et CLI `mimicctl` | fait, validé sur kind avec la démo OpenTelemetry |
| P6d | Parcours SRE : garde-fou production, `capture.inbound`, `selector`, label `version` des métriques | fait (étape kind E2E-04 ajoutée, non jouée en local) |
| P6e | Mock centralisé : plus d'import entre sidecars sous contrôleur, un exemple rejouable par endpoint gardé hors rétention, historique de 24 heures par défaut | fait |
| P8b | Tir de charge intégré `mimicctl load` : rejeu des requêtes enregistrées, concurrence fixe ou montée jusqu'à saturation, recommandation de CPU et de mémoire | fait |
| P6f | Snapshot de mock léger : un pod en mock télécharge la forme de mock du snapshot (rejouables et durées par opération), compressée, identique sur tous les pods ; 47,4 Mo → 0,29 Mo sur le banc d'arrivée d'un pod | fait |

## Lancer le sidecar en local

```sh
cargo run -p mimic-proxy -- --config exemple.yaml
```

```yaml
mode: record
inbound:
  listen: 127.0.0.1:15106
  upstream: 127.0.0.1:8080   # l'application
outbound:
  listen: 127.0.0.1:15101    # à utiliser comme HTTP_PROXY par l'application
admin:
  listen: 127.0.0.1:15100    # page d'admin sur http://127.0.0.1:15100/
```

Bascule de mode : `curl -X PUT --data mock http://127.0.0.1:15100/mode`.

Déploiement dans Kubernetes : voir [deploy/README.md](deploy/README.md).

## mimicctl

Référence complète, droits nécessaires et interface web du contrôleur :
[docs/guide/CLI-mimicctl.md](docs/guide/CLI-mimicctl.md).
Tir de charge d'un workload isolé, métriques à suivre et exemple blue/green avec Argo Rollouts :
[docs/sre-load-test.md](docs/sre-load-test.md).

```sh
cargo install --path crates/mimicctl
mimicctl mode checkout mock -n shop          # bascule tous les pods du workload
mimicctl status -A                           # mode souhaité, snapshot, pods alignés
mimicctl export checkout -n shop --inbound --openapi-version 3.0 -o checkout.yaml
mimicctl requests checkout -n shop --format k6 > replay.js
mimicctl load checkout -n perf --target http://checkout.perf:8080 --ramp   # capacité et taille
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

## Licence

[Apache 2.0](LICENSE).
