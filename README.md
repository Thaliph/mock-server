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
| P2 | Proxy HTTP en clair, mode record | à faire |
| P3 | Matching et mode mock | à faire |

## Développement

```sh
cargo test --workspace          # tests unitaires, propriétés et snapshots
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all --check
cargo insta review              # après un changement voulu de la sortie OpenAPI
```

Les tests portent l'identifiant du critère d'acceptation qu'ils couvrent
(`inf_01_…`, `red_04_…`), voir la section 11 de la spec.
