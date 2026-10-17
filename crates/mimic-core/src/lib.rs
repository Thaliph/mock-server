//! Logique métier du sidecar mimic, sans I/O : modèle des échanges capturés,
//! inférence OpenAPI et redaction des données sensibles.
//!
//! Voir `docs/SPEC.md` pour la spécification et les critères d'acceptation.

pub mod infer;
pub mod model;
pub mod redact;
