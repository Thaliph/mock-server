//! Le runbook du sidecar (`docs/runbook/sidecar.md`) couvre chaque message que le sidecar
//! montre à l'exploitant (DOC-12), sur le modèle de DOC-11 (contrôleur).
//!
//! La règle d'extraction est commune (`tests/support/runbook.rs`). Propre au sidecar :
//! `eprintln!` (sortie du processus), `refuse_tunnel(…, "…")` (le `warn!` y journalise
//! `{message}`), le couple `(&self.…_log, "…")` du relevé des requêtes sans réponse, et le
//! corps du `599` du mock sans réponse. Les exclusions volontaires, avec leur raison, sont dans
//! `runbook-exclusions.txt` : ainsi `{}: {}` n'a aucun texte fixe, c'est l'erreur d'E/S de l'OS
//! préfixée par un chemin de configuration, que la page ne peut pas énumérer.

#[path = "../../../tests/support/runbook.rs"]
mod runbook;

use runbook::Config;

/// Messages sans littéral dans le code, à documenter à la main :
/// - `admin.rs` `json_response` : corps `500` = erreur de `serde_json` (« sérialisation JSON ») ;
/// - `config.rs` module `duration_secs` : durée négative, non finie ou trop grande, rejetée par
///   `Duration::try_from_secs_f64` (« cannot convert float seconds to Duration »).
const DYNAMIC_MESSAGES: [&str; 2] = [
    "sérialisation JSON",
    "cannot convert float seconds to Duration",
];

/// Le 599 du mock : `StatusCode::from_u16(599).expect("599 est un code valide"),`.
fn is_mock_599(pre: &str) -> bool {
    pre.ends_with("expect(\"599 est un code valide\"),")
}

/// Journal d'un appelant : `(&self.…_log, "…")`.
fn is_log_argument(pre: &str) -> bool {
    pre.ends_with("_log,")
}

const CONFIG: Config = Config {
    sources: "crates/mimic-proxy/src",
    runbook: "docs/runbook/sidecar.md",
    exclusions: include_str!("runbook-exclusions.txt"),
    last_literal_calls: &[
        "tracing::warn!(",
        "tracing::error!(",
        "eprintln!(",
        "refuse_tunnel(",
    ],
    extra: &[is_mock_599, is_log_argument],
    dynamic: &DYNAMIC_MESSAGES,
    generic_prefix_is_whole: true,
    word_http_bodies: true,
};

#[test]
fn doc_12_extraction_finds_the_known_messages() {
    runbook::assert_extraction_finds(
        &CONFIG,
        &[
            "battement vers le contrôleur échoué",
            "annotation de mode invalide",
            "plafond de connexions simultanées atteint, connexion refusée",
            "mimic: aucun enregistrement ne correspond à cette requête",
            "mimic: body trop gros pour le mode mock",
            "mimic: amont injoignable",
            "mimic: connexion vers l'amont non établie dans le délai (outbound.connect_timeout)",
            "connexion sortante muette refusée en mode mock (mock.tcp: refuse)",
            "mock.latency_factor doit être entre 0 et 100",
            "redact.salt requis sans contrôleur : fournir MIMIC_REDACT_SALT (Secret <workload>-mimic, clé redact-salt) ou redact.salt",
            "share.ca_file requis : seule la CA du contrôleur fait foi",
            "dépendance inconnue",
            "store {}: {}",
            "mimic: sonde",
            "introuvable",
            "partage désactivé",
        ],
        40,
    );
}

#[test]
fn doc_12_every_exclusion_is_a_real_message() {
    runbook::assert_exclusions_are_real(&CONFIG);
}

/// Chaque message du sidecar figure dans une ligne de tableau de `docs/runbook/sidecar.md`
/// qui donne aussi la cause, l'effet et l'action (DOC-12).
#[test]
fn doc_12_every_sidecar_message_is_in_the_runbook() {
    runbook::assert_every_message_is_in_the_runbook(&CONFIG);
}
