//! Le runbook du contrôleur (`docs/runbook/controller.md`) couvre chaque message que le
//! contrôleur montre à l'exploitant (DOC-11).
//!
//! La règle d'extraction est commune (`tests/support/runbook.rs`). Propre au contrôleur : les
//! erreurs d'ingestion `IngestError::Storage(format!("…"))` / `Invalid(…)`, les messages d'erreur
//! boxés d'un `.map_err(|e| Box::new(…))`, et le texte d'un `message:` de `StatusCondition`
//! (`Some(…)`, `Some(format!(…))`, `.then(|| …)`). Les exclusions volontaires, avec leur
//! raison, sont dans `runbook-exclusions.txt`.

#[path = "../../../tests/support/runbook.rs"]
mod runbook;

use runbook::Config;

/// Littéral de `IngestError::Storage(` / `IngestError::Invalid(`.
fn is_ingest_error(pre: &str) -> bool {
    pre.ends_with("IngestError::Storage(") || pre.ends_with("IngestError::Invalid(")
}

/// Littéral qui ouvre le champ `message:` d'une `StatusCondition`.
fn is_status_message(pre: &str) -> bool {
    pre.rsplit_once("message:").is_some_and(|(_, tail)| {
        let tail: String = tail.chars().filter(|c| !c.is_whitespace()).collect();
        matches!(tail.as_str(), "Some(" | "empty.then(||")
    })
}

/// Littéral de `.map_err(|e| Box::new(…))`.
fn is_boxed_map_err(pre: &str) -> bool {
    pre.strip_suffix("Box::new(").is_some_and(|head| {
        let head = head.strip_suffix('{').unwrap_or(head).trim_end();
        head.strip_suffix('|')
            .and_then(|h| h.rsplit_once('|'))
            .is_some_and(|(open, _)| open.ends_with("map_err("))
    })
}

const CONFIG: Config = Config {
    sources: "crates/mimic-controller/src",
    runbook: "docs/runbook/controller.md",
    exclusions: include_str!("runbook-exclusions.txt"),
    last_literal_calls: &["tracing::warn!(", "tracing::error!("],
    extra: &[is_ingest_error, is_status_message, is_boxed_map_err],
    dynamic: &[],
    generic_prefix_is_whole: false,
    word_http_bodies: false,
};

#[test]
fn doc_11_extraction_finds_the_known_messages() {
    runbook::assert_extraction_finds(
        &CONFIG,
        &[
            "bail non renouvelé à temps : la réplique cesse de servir",
            "statut non publié",
            "support indisponible",
            "jeton absent ou invalide",
            "store non supporté:",
            "client Kubernetes:",
            "battement invalide:",
            "route inconnue",
            "X-Remote-User absent",
            "kube-system/extension-apiserver-authentication:",
        ],
        50,
    );
}

#[test]
fn doc_11_every_exclusion_is_a_real_message() {
    runbook::assert_exclusions_are_real(&CONFIG);
}

/// Chaque message du contrôleur figure dans une ligne de tableau de `docs/runbook/controller.md`
/// qui donne aussi la cause, l'effet et l'action (DOC-11).
#[test]
fn doc_11_every_controller_message_is_in_the_runbook() {
    runbook::assert_every_message_is_in_the_runbook(&CONFIG);
}
