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
| P5 | Interception TLS sortante | à faire |
| P7 | Webhook d'injection et `mimicctl` | à faire |

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

## Développement

```sh
cargo test --workspace          # tests unitaires, propriétés et snapshots
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo insta review              # après un changement voulu de la sortie OpenAPI
sudo ./scripts/e2e-netns.sh     # interception iptables réelle, dans un namespace réseau jetable
```

Les tests portent l'identifiant du critère d'acceptation qu'ils couvrent
(`inf_01_…`, `red_04_…`), voir la section 11 de la spec.
