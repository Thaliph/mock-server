# Instructions pour Claude Code

- La spécification fait foi : `docs/SPEC.md`. Toute évolution de comportement passe d'abord par la spec.
- TDD : pour chaque phase, écrire les tests d'acceptation (identifiants de la section 11.2 dans le nom du test) avant l'implémentation.
- `mimic-core` reste pur : pas de tokio, pas d'I/O, pas d'horloge. Toute la logique testable y vit ; le proxy n'est qu'un adaptateur.
- L'inférence doit rester déterministe (INF-08) : collections ordonnées (`BTreeMap`, `BTreeSet`), aucun `HashMap` dans une sortie.
- Aucune valeur sensible ne doit atteindre le disque sans passer par `mimic_core::redact` (RED-04).
- Avant de proposer une PR : `cargo fmt --all --check`, `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test --workspace`.
- Jamais de push direct sur `main`.
- Code et commentaires en français, identifiants en anglais.
