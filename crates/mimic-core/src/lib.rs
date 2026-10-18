//! Logique métier du sidecar mimic, sans I/O : modèle des échanges capturés,
//! inférence OpenAPI, moteur de mock, redaction des données sensibles, échantillonnage des
//! bodies, snapshots, rejeu, compaction des segments partagés, conversion OpenAPI 3.0 et
//! diff de specs, élection de la réplique active du contrôleur.
//!
//! Voir `docs/SPEC.md` pour la spécification et les critères d'acceptation.

pub mod catalog;
pub mod compact;
pub mod diff;
pub mod exemplar;
pub mod infer;
pub mod lease;
pub mod mock;
pub mod model;
pub mod openapi;
pub mod redact;
pub mod replay;
pub mod sample;
pub mod snapshot;
