//! Plafond de bodies par opération (spec §5.2) et compaction des segments partagés
//! (spec §5.5.2). Les échanges traités ici sont déjà redactés : on ne fait qu'en retirer.

use std::collections::{BTreeMap, BTreeSet};

use crate::model::{Body, Exchange};

/// Clé du plafond de bodies : sens, authority, méthode, chemin et code retour.
pub fn body_key(e: &Exchange) -> String {
    format!(
        "{:?}|{}|{}|{}|{}",
        e.direction, e.authority, e.method, e.path, e.status
    )
}

/// Vrai si la requête ou la réponse porte un body, même tronqué.
pub fn has_body(e: &Exchange) -> bool {
    [&e.req_body, &e.resp_body]
        .iter()
        .any(|b| matches!(b, Body::Captured { .. } | Body::Truncated { .. }))
}

/// Remplace les bodies par leur seule taille.
pub fn strip_bodies(e: &mut Exchange) {
    for b in [&mut e.req_body, &mut e.resp_body] {
        let size = match b {
            Body::Captured { data } => data.len() as u64,
            Body::Truncated { size, .. } => *size,
            _ => continue,
        };
        *b = Body::Skipped { size };
    }
}

/// Places de bodies par couple (opération, code) : les `max` premiers échanges gardent les
/// leurs, les suivants sont gardés sans body jusqu'à ce qu'une éviction libère une place.
#[derive(Debug, Clone, Default)]
pub struct BodySamples {
    max: usize,
    counts: BTreeMap<String, usize>,
}

impl BodySamples {
    pub fn new(max: usize) -> Self {
        Self {
            max,
            counts: BTreeMap::new(),
        }
    }

    /// Prend une place pour les bodies de `e`, ou les retire s'il n'en reste pas.
    /// Vrai si l'échange garde ses bodies (ou n'en a pas).
    pub fn admit(&mut self, e: &mut Exchange) -> bool {
        if !has_body(e) {
            return true;
        }
        let n = self.counts.entry(body_key(e)).or_default();
        if *n >= self.max {
            strip_bodies(e);
            return false;
        }
        *n += 1;
        true
    }

    /// Rend la place d'un échange évincé.
    pub fn release(&mut self, e: &Exchange) {
        if !has_body(e) {
            return;
        }
        let key = body_key(e);
        if let Some(n) = self.counts.get_mut(&key) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                self.counts.remove(&key);
            }
        }
    }
}

/// Fusion de segments JSONL en un seul : échanges dédoublonnés par identifiant (la première
/// occurrence gagne), au plus `samples` bodies par couple (opération, code). Le résultat ne
/// dépend que des segments et de leur ordre.
#[derive(Debug)]
pub struct Compactor {
    ids: BTreeSet<String>,
    bodies: BodySamples,
    out: Vec<u8>,
}

impl Compactor {
    pub fn new(samples: usize) -> Self {
        Self {
            ids: BTreeSet::new(),
            bodies: BodySamples::new(samples),
            out: Vec::new(),
        }
    }

    /// Ajoute les lignes d'un segment ; une ligne illisible est ignorée.
    /// Rend le nombre d'échanges ajoutés.
    pub fn push(&mut self, segment: &[u8]) -> usize {
        let mut added = 0;
        for line in segment.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            let Ok(mut e) = serde_json::from_slice::<Exchange>(line) else {
                continue;
            };
            if self.ids.contains(&e.id) {
                continue;
            }
            self.bodies.admit(&mut e);
            let Ok(line) = serde_json::to_vec(&e) else {
                continue;
            };
            self.ids.insert(e.id);
            self.out.extend_from_slice(&line);
            self.out.push(b'\n');
            added += 1;
        }
        added
    }

    pub fn len(&self) -> usize {
        self.ids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
    }

    /// Segment fusionné, un échange par ligne.
    pub fn finish(self) -> Vec<u8> {
        self.out
    }
}

const HOUR_MS: i64 = 3_600_000;

/// Heure UTC qui contient la date `ms` (ms Unix) : nom `AAAAMMJJHH` et bornes `[début, fin)`.
pub fn hour_of(ms: i64) -> (String, i64, i64) {
    let start = ms.div_euclid(HOUR_MS) * HOUR_MS;
    let hours = start / HOUR_MS;
    let (y, m, d) = civil_from_days(hours.div_euclid(24));
    let name = format!("{y:04}{m:02}{d:02}{:02}", hours.rem_euclid(24));
    (name, start, start + HOUR_MS)
}

/// Début (ms Unix) de l'heure UTC nommée `AAAAMMJJHH` ; `None` si le nom est invalide.
pub fn hour_start(name: &str) -> Option<i64> {
    if name.len() != 10 || !name.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let num = |r: std::ops::Range<usize>| name[r].parse::<i64>().ok();
    let (y, m, d, h) = (num(0..4)?, num(4..6)?, num(6..8)?, num(8..10)?);
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || h > 23 {
        return None;
    }
    let days = days_from_civil(y, m as u32, d as u32);
    let start = (days * 24 + h) * HOUR_MS;
    // Rejette les dates impossibles (31 février) : le nom doit se relire à l'identique.
    (hour_of(start).0 == name).then_some(start)
}

/// Nombre de jours depuis le 1er janvier 1970 d'une date grégorienne (`days_from_civil`).
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Date grégorienne d'un nombre de jours depuis le 1er janvier 1970 (algorithme de
/// H. Hinnant, `civil_from_days`).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}
