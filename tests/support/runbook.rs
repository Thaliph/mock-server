//! Extracteur commun des tests DOC : chaque message que le code d'un composant montre à
//! l'exploitant figure dans une ligne de tableau de son runbook (DOC-11 contrôleur, DOC-12
//! sidecar). Inclus par chaque `runbook_doc.rs` avec `#[path]` ; ce fichier n'est pas un
//! crate, il est compilé dans chaque binaire de test qui l'inclut.
//!
//! Règle d'extraction, volontairement simple : dans les sources du composant, on lit le code
//! hors tests (tout ce qui précède `#[cfg(test)]`, commentaires de ligne retirés) et on retient
//! les littéraux de chaîne qui sont :
//!
//! 1. le dernier littéral d'un appel de `Config::last_literal_calls` (`tracing::warn!(`,
//!    `tracing::error!(`, `eprintln!(`, `refuse_tunnel(`…) ;
//! 2. le corps d'une réponse HTTP d'erreur : littéral qui suit `StatusCode::XXX,` (hors 2xx),
//!    éventuellement précédé de `format!(` ou du nom de raison `"BadRequest",` ;
//! 3. un message d'erreur de démarrage ou de support : `Err("…".into())`, `Err(format!("…"))`,
//!    `.map_err(|e| format!("…"))`, `.ok_or("…")`, `.ok_or_else(|| …)` ;
//! 4. un littéral que reconnaît un motif propre au composant (`Config::extra`).
//!
//! Hors périmètre : les `info!` et `debug!` (journal de fonctionnement).
//!
//! De chaque littéral, on garde le texte fixe : celui qui précède le premier `{…}`
//! (placeholder de `format!`), ou, s'il est vide, le plus long segment fixe, ou, si ce n'est
//! qu'un mot isolé, le littéral entier avec ses placeholders écrits `{}`. Ce texte doit figurer
//! dans la PREMIÈRE cellule d'une ligne de tableau de la page, qui donne au moins le message,
//! la cause, l'effet et l'action : citer un texte dans la cause ou l'action d'une autre ligne
//! ne le couvre pas. Les exclusions volontaires, avec leur raison, sont dans le fichier
//! `runbook-exclusions.txt` du composant.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Ce qui change d'un composant à l'autre.
pub struct Config<'a> {
    /// Dossier des sources, relatif à la racine du dépôt (`crates/mimic-proxy/src`).
    pub sources: &'a str,
    /// Page du runbook, relative à la racine du dépôt.
    pub runbook: &'a str,
    /// Contenu de `runbook-exclusions.txt` : `texte fixe | raison`, une par ligne.
    pub exclusions: &'a str,
    /// Appels dont le dernier littéral est un message (`tracing::warn!(`…).
    pub last_literal_calls: &'a [&'a str],
    /// Motifs propres au composant : reçoivent le texte qui précède un littéral, vrai si ce
    /// littéral est un message.
    pub extra: &'a [fn(&str) -> bool],
    /// Messages sans littéral dans le code, à documenter à la main.
    pub dynamic: &'a [&'a str],
    /// Un préfixe fixe d'un seul mot (`store {}: …`) est trop générique : on garde le littéral
    /// entier, placeholders écrits `{}`. Faux pour le contrôleur, dont la page cite le mot seul
    /// (règle historique de DOC-11, à ne pas changer ici).
    pub generic_prefix_is_whole: bool,
    /// Un mot seul peut être un corps d'erreur HTTP (`StatusCode::NOT_FOUND, "introuvable"`),
    /// alors qu'un mot seul est sinon pris pour un nom de raison d'API (`"BadRequest"`).
    /// Faux pour le contrôleur (règle historique de DOC-11).
    pub word_http_bodies: bool,
}

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Un littéral de chaîne : indices (en caractères) de début et de fin, et valeur.
struct Lit {
    start: usize,
    end: usize,
    value: String,
}

/// Code de production : sans commentaires de ligne et sans les éléments `#[cfg(test)]`
/// (l'élément ou le `mod tests {…}` qui suit l'attribut, pas le reste du fichier).
fn production_code(source: &str) -> String {
    let code: Vec<char> = source
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
        .chars()
        .collect();
    let attr: Vec<char> = "#[cfg(test)]".chars().collect();
    let mut out = String::new();
    let mut i = 0;
    while i < code.len() {
        if code[i..].starts_with(&attr) {
            i = item_end(&code, i + attr.len());
        } else {
            out.push(code[i]);
            i += 1;
        }
    }
    out
}

/// Indice qui suit l'élément commençant à `from` : jusqu'au `;` ou au bloc `{…}` apparié
/// (chaînes et littéraux de caractère ignorés).
fn item_end(code: &[char], from: usize) -> usize {
    let mut depth = 0;
    let mut i = from;
    while i < code.len() {
        match code[i] {
            '"' => {
                i += 1;
                while i < code.len() && code[i] != '"' {
                    i += if code[i] == '\\' { 2 } else { 1 };
                }
            }
            '\'' if code.get(i + 2) == Some(&'\'') => i += 2,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return i + 1;
                }
            }
            ';' if depth == 0 => return i + 1,
            _ => {}
        }
        i += 1;
    }
    code.len()
}

/// Première cellule d'une ligne de tableau `| message | cause | effet | action |`.
fn first_cell(row: &str) -> &str {
    row.split('|').nth(1).unwrap_or("")
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
                        match code.get(i) {
                            // Continuation de ligne : l'indentation suivante est ignorée.
                            Some('\n') => {
                                while code.get(i).is_some_and(|c| c.is_whitespace()) {
                                    i += 1;
                                }
                                continue;
                            }
                            Some('n') => value.push('\n'),
                            Some(&c) => value.push(c),
                            None => break,
                        }
                        i += 1;
                        continue;
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
fn fixed_text(literal: &str, cfg: &Config) -> String {
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
        // Un préfixe d'un seul mot (`store {}: …`) est trop générique : littéral entier.
        if cfg.generic_prefix_is_whole && first.split_whitespace().count() < 2 && segments.len() > 1
        {
            return normalized_placeholders(literal);
        }
        return first;
    }
    let cleaned: Vec<String> = segments.iter().map(|s| clean(s)).collect();
    let longest = cleaned
        .iter()
        .map(String::as_str)
        .max_by_key(|s| s.chars().count())
        .unwrap_or("");
    // Un mot isolé (`vide`) est trop générique : on garde le littéral entier, placeholders
    // normalisés en `{}` (`{} vide`), pour que la ligne du runbook soit sans ambiguïté.
    if longest.split_whitespace().count() < 2 {
        return normalized_placeholders(literal);
    }
    longest.to_string()
}

/// Littéral dont chaque `{…}` est remplacé par `{}`.
fn normalized_placeholders(literal: &str) -> String {
    let mut out = String::new();
    let mut depth = 0;
    for c in literal.chars() {
        match c {
            '{' => {
                if depth == 0 {
                    out.push_str("{}");
                }
                depth += 1;
            }
            '}' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    out.trim().to_string()
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
        .is_some_and(|t| {
            t.starts_with("StatusCode::")
                && !["OK", "CREATED", "ACCEPTED"].contains(&&t["StatusCode::".len()..])
        })
}

fn is_closure_of_map_err(pre: &str) -> bool {
    let pre = pre.strip_suffix('{').unwrap_or(pre).trim_end();
    let Some(head) = pre.strip_suffix('|') else {
        return false;
    };
    head.rsplit_once('|')
        .is_some_and(|(open, _)| open.ends_with("map_err("))
}

fn is_error_return(pre: &str, cfg: &Config) -> bool {
    ["Err(", ".ok_or(", ".ok_or_else(||"]
        .iter()
        .any(|p| pre.ends_with(p))
        || is_closure_of_map_err(pre)
        || cfg.extra.iter().any(|f| f(pre))
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

/// Messages (texte fixe) d'une source de production, selon les règles du module.
fn messages_of(source: &str, cfg: &Config) -> BTreeSet<String> {
    let chars: Vec<char> = production_code(source).chars().collect();
    let lits = literals(&chars);
    let mut found = BTreeSet::new();
    let mut keep = |value: &str, http_body: bool| {
        // Un nom de raison d'API (`"BadRequest"`) n'est pas un message, mais un mot seul peut
        // être un corps d'erreur HTTP (`StatusCode::NOT_FOUND, "introuvable"`).
        if (cfg.word_http_bodies && http_body) || !value.chars().all(|c| c.is_ascii_alphabetic()) {
            let fixed = fixed_text(value, cfg);
            if !fixed.is_empty() {
                found.insert(fixed);
            }
        }
    };

    // Règle 1 : dernier littéral d'un appel journalisant.
    for name in cfg.last_literal_calls {
        let pattern: Vec<char> = name.chars().collect();
        for at in (0..chars.len()).filter(|&i| chars[i..].starts_with(&pattern)) {
            let open = at + pattern.len() - 1;
            let end = call_end(&chars, open);
            if let Some(message) = lits.iter().rfind(|l| l.start > open && l.end <= end) {
                keep(&message.value, false);
            }
        }
    }

    // Règles 2 à 4 : position du littéral dans l'appel.
    for (n, lit) in lits.iter().enumerate() {
        let pre = before(&chars, lit.start);
        let after_reason = n > 0 && {
            let prev = &lits[n - 1];
            pre.ends_with("\",")
                && prev.value.chars().all(|c| c.is_ascii_alphabetic())
                && after_status_code(&before(&chars, prev.start))
        };
        // Corps d'erreur HTTP : suit le code, sans autre littéral derrière (sinon c'est le nom
        // de raison, et le corps est le littéral suivant).
        let tail: String = chars[lit.end..].iter().take(12).collect();
        let tail = tail.trim_start().trim_start_matches(',').trim_start();
        let is_reason = tail.starts_with('"') || tail.starts_with("format!(");
        let http_body = after_reason || (after_status_code(&pre) && !is_reason);
        if after_status_code(&pre) || after_reason || is_error_return(&pre, cfg) {
            keep(&lit.value, http_body);
        }
    }
    found
}

/// Messages (texte fixe) de tous les fichiers `.rs` des sources du composant.
pub fn messages(cfg: &Config) -> BTreeSet<String> {
    let mut files: Vec<_> = std::fs::read_dir(root().join(cfg.sources))
        .unwrap()
        .map(|e| e.unwrap().path())
        .filter(|p| p.extension().is_some_and(|e| e == "rs"))
        .collect();
    files.sort();
    files
        .iter()
        .flat_map(|f| messages_of(&std::fs::read_to_string(f).unwrap(), cfg))
        .collect()
}

/// Exclusions : `texte fixe | raison`, une par ligne ; `#` ouvre un commentaire.
fn exclusions(cfg: &Config) -> BTreeSet<String> {
    cfg.exclusions
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(|l| {
            let (text, reason) = l.split_once(" | ").expect("« texte | raison »");
            assert!(!reason.trim().is_empty(), "raison manquante : {l}");
            text.trim().to_string()
        })
        .collect()
}

/// L'extraction retrouve les messages connus et n'est pas anormalement pauvre.
pub fn assert_extraction_finds(cfg: &Config, known: &[&str], min_len: usize) {
    let found = messages(cfg);
    for known in known {
        assert!(
            found.contains(*known),
            "message non extrait : {known}\n{found:#?}"
        );
    }
    assert!(
        found.len() > min_len,
        "extraction trop pauvre : {}",
        found.len()
    );
}

/// Chaque exclusion désigne un message réellement extrait (pas d'exclusion périmée).
pub fn assert_exclusions_are_real(cfg: &Config) {
    let found = messages(cfg);
    for excluded in exclusions(cfg) {
        assert!(
            found.contains(&excluded),
            "exclusion périmée (plus de tel message) : {excluded}"
        );
    }
}

/// Chaque message (extrait ou dynamique) figure dans la première cellule d'une ligne de
/// tableau du runbook qui donne aussi la cause, l'effet et l'action.
pub fn assert_every_message_is_in_the_runbook(cfg: &Config) {
    let page = std::fs::read_to_string(root().join(cfg.runbook))
        .unwrap_or_else(|e| panic!("{} manquant : {e}", cfg.runbook));
    let rows: Vec<&str> = page.lines().filter(|l| l.starts_with('|')).collect();
    let excluded = exclusions(cfg);

    let mut missing = Vec::new();
    let mut incomplete = Vec::new();
    let mut all = messages(cfg);
    all.extend(cfg.dynamic.iter().map(|m| m.to_string()));
    for message in all.difference(&excluded) {
        match rows
            .iter()
            .find(|r| first_cell(r).contains(message.as_str()))
        {
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
