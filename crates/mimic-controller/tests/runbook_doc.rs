//! Le runbook du contrôleur (`docs/runbook/controller.md`) couvre chaque message que le
//! contrôleur montre à l'exploitant (DOC-10).
//!
//! Règle d'extraction, volontairement simple : dans `crates/mimic-controller/src/*.rs`, on lit
//! le code hors tests (tout ce qui précède `#[cfg(test)]`, commentaires de ligne retirés) et on
//! retient les littéraux de chaîne qui sont :
//!
//! 1. le message d'un `tracing::warn!` ou `tracing::error!` (dernier littéral de l'appel : les
//!    champs structurés `error = %e` n'en contiennent pas) ;
//! 2. le corps d'une réponse HTTP d'erreur : littéral qui suit `StatusCode::XXX,` (éventuellement
//!    précédé de `format!(` ou du nom de raison `"BadRequest",` de l'API agrégée) ;
//! 3. un message d'erreur de démarrage ou de support : littéral de `Err("…".into())`,
//!    `Err(format!("…"))`, `Err(IngestError::…(format!("…")))` ou de `.map_err(|e| format!("…"))`.
//!
//! De chaque littéral, on garde le texte fixe : celui qui précède le premier `{…}` (placeholder
//! de `format!`), ou, s'il est vide, le plus long segment fixe. Ce texte doit figurer dans une
//! ligne de tableau de la page, qui donne au moins le message, la cause, l'effet et l'action.
//! Les exclusions volontaires, avec leur raison, sont dans `runbook-exclusions.txt`.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

const RUNBOOK: &str = "docs/runbook/controller.md";
const EXCLUSIONS: &str = include_str!("runbook-exclusions.txt");

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Un littéral de chaîne : indices (en caractères) de début et de fin, et valeur.
struct Lit {
    start: usize,
    end: usize,
    value: String,
}

/// Code hors tests, commentaires de ligne retirés.
fn production_code(source: &str) -> String {
    let code = source.split("#[cfg(test)]").next().unwrap_or(source);
    code.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn literals(code: &[char]) -> Vec<Lit> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < code.len() {
        match code[i] {
            // Littéral de caractère `'"'` : pas un début de chaîne.
            '\'' if code.get(i + 1) == Some(&'"') && code.get(i + 2) == Some(&'\'') => i += 3,
            '"' => {
                let start = i;
                let mut value = String::new();
                i += 1;
                while i < code.len() && code[i] != '"' {
                    if code[i] == '\\' {
                        i += 1;
                    }
                    value.push(code[i]);
                    i += 1;
                }
                i += 1;
                out.push(Lit {
                    start,
                    end: i,
                    value,
                });
            }
            _ => i += 1,
        }
    }
    out
}

/// Texte fixe d'un littéral : avant le premier `{…}`, sinon le plus long segment fixe.
fn fixed_text(literal: &str) -> String {
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut depth = 0;
    for c in literal.chars() {
        match c {
            '{' => {
                if depth == 0 {
                    segments.push(std::mem::take(&mut current));
                }
                depth += 1;
            }
            '}' if depth > 0 => depth -= 1,
            _ if depth == 0 => current.push(c),
            _ => {}
        }
    }
    segments.push(current);
    // Une ponctuation de tête (`{}: …`, `{}) : …`) est un reste du placeholder.
    let clean = |s: &str| s.trim_start_matches([':', ')', ' ']).trim().to_string();
    let first = clean(&segments[0]);
    if !first.is_empty() {
        return first;
    }
    let cleaned: Vec<String> = segments.iter().map(|s| clean(s)).collect();
    cleaned
        .iter()
        .map(String::as_str)
        .max_by_key(|s| s.chars().count())
        .unwrap_or("")
        .to_string()
}

/// Texte qui précède `start`, sans `format!(` ni espaces de fin.
fn before(code: &[char], start: usize) -> String {
    let text: String = code[..start].iter().collect();
    let text = text.trim_end();
    text.strip_suffix("format!(")
        .unwrap_or(text)
        .trim_end()
        .to_string()
}

fn after_status_code(pre: &str) -> bool {
    pre.strip_suffix(',')
        .and_then(|p| p.rsplit(|c: char| c == '(' || c.is_whitespace()).next())
        .is_some_and(|t| t.starts_with("StatusCode::"))
}

fn is_closure_of_map_err(pre: &str) -> bool {
    let pre = pre.strip_suffix('{').unwrap_or(pre).trim_end();
    let Some(head) = pre.strip_suffix('|') else {
        return false;
    };
    head.rsplit_once('|')
        .is_some_and(|(open, _)| open.ends_with("map_err("))
}

fn is_error_return(pre: &str) -> bool {
    ["Err(", "IngestError::Storage(", "IngestError::Invalid("]
        .iter()
        .any(|p| pre.ends_with(p))
        || is_closure_of_map_err(pre)
        || pre
            .strip_suffix("Box::new(")
            .is_some_and(is_closure_of_map_err)
}

/// Indice de la parenthèse fermante de l'appel dont la parenthèse ouvrante est à `open`.
fn call_end(code: &[char], open: usize) -> usize {
    let mut depth = 0;
    let mut in_string = false;
    let mut i = open;
    while i < code.len() {
        match code[i] {
            '\\' if in_string => i += 1,
            '"' => in_string = !in_string,
            '(' if !in_string => depth += 1,
            ')' if !in_string => {
                depth -= 1;
                if depth == 0 {
                    return i;
                }
            }
            _ => {}
        }
        i += 1;
    }
    code.len()
}

/// Messages (texte fixe) d'une source de production, selon les trois règles du module.
fn messages_of(source: &str) -> BTreeSet<String> {
    let chars: Vec<char> = production_code(source).chars().collect();
    let lits = literals(&chars);
    let mut found = BTreeSet::new();
    let mut keep = |value: &str| {
        // Un nom de raison d'API (`"BadRequest"`) n'est pas un message.
        if !value.chars().all(|c| c.is_ascii_alphabetic()) {
            let fixed = fixed_text(value);
            if !fixed.is_empty() {
                found.insert(fixed);
            }
        }
    };

    // Règle 1 : dernier littéral d'un tracing::warn!/error!.
    for name in ["tracing::warn!(", "tracing::error!("] {
        let pattern: Vec<char> = name.chars().collect();
        for at in (0..chars.len()).filter(|&i| chars[i..].starts_with(&pattern)) {
            let open = at + pattern.len() - 1;
            let end = call_end(&chars, open);
            if let Some(message) = lits.iter().rfind(|l| l.start > open && l.end <= end) {
                keep(&message.value);
            }
        }
    }

    // Règles 2 et 3 : position du littéral dans l'appel.
    for (n, lit) in lits.iter().enumerate() {
        let pre = before(&chars, lit.start);
        let after_reason = n > 0 && {
            let prev = &lits[n - 1];
            pre.ends_with("\",")
                && prev.value.chars().all(|c| c.is_ascii_alphabetic())
                && after_status_code(&before(&chars, prev.start))
        };
        if after_status_code(&pre) || after_reason || is_error_return(&pre) {
            keep(&lit.value);
        }
    }
    found
}

fn controller_messages() -> BTreeSet<String> {
    let src = root().join("crates/mimic-controller/src");
    let mut files: Vec<_> = std::fs::read_dir(&src)
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "rs"))
        .collect();
    files.sort();
    files
        .iter()
        .flat_map(|f| messages_of(&std::fs::read_to_string(f).unwrap()))
        .collect()
}

/// Exclusions : `texte fixe | raison`, une par ligne ; `#` ouvre un commentaire.
fn exclusions() -> BTreeSet<String> {
    EXCLUSIONS
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| {
            let (text, reason) = l.split_once(" | ").expect("« texte | raison »");
            assert!(!reason.trim().is_empty(), "raison manquante : {l}");
            text.trim().to_string()
        })
        .collect()
}

#[test]
fn doc_10_extraction_finds_the_known_messages() {
    let found = controller_messages();
    for known in [
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
    ] {
        assert!(
            found.contains(known),
            "message non extrait : {known}\n{found:#?}"
        );
    }
    assert!(found.len() > 50, "extraction trop pauvre : {}", found.len());
}

#[test]
fn doc_10_every_exclusion_is_a_real_message() {
    let found = controller_messages();
    for excluded in exclusions() {
        assert!(
            found.contains(&excluded),
            "exclusion périmée (plus de tel message) : {excluded}"
        );
    }
}

/// Chaque message du contrôleur figure dans une ligne de tableau de `docs/runbook/controller.md`
/// qui donne aussi la cause, l'effet et l'action (DOC-10).
#[test]
fn doc_10_every_controller_message_is_in_the_runbook() {
    let page = std::fs::read_to_string(root().join(RUNBOOK))
        .unwrap_or_else(|e| panic!("{RUNBOOK} manquant : {e}"));
    let rows: Vec<&str> = page.lines().filter(|l| l.starts_with('|')).collect();
    let excluded = exclusions();

    let mut missing = Vec::new();
    let mut incomplete = Vec::new();
    let messages = controller_messages();
    for message in messages.difference(&excluded) {
        match rows.iter().find(|r| r.contains(message.as_str())) {
            None => missing.push(message.as_str()),
            Some(row) => {
                let cells = row.split('|').filter(|c| !c.trim().is_empty()).count();
                if cells < 4 {
                    incomplete.push(message.as_str());
                }
            }
        }
    }
    assert!(missing.is_empty(), "absents du runbook :\n{missing:#?}");
    assert!(
        incomplete.is_empty(),
        "lignes sans message, cause, effet et action :\n{incomplete:#?}"
    );
}
