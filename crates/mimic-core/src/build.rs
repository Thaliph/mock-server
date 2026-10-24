//! Identité de la construction, exposée par `mimic_build_info` (spec §9.3).

/// Version du workspace.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

/// Commit de la construction : `MIMIC_REVISION` à la compilation (posée par l'image),
/// sinon `unknown`.
pub const REVISION: &str = match option_env!("MIMIC_REVISION") {
    Some(r) if !r.is_empty() => r,
    _ => "unknown",
};
