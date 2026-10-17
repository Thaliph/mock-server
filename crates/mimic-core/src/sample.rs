//! Échantillonnage des bodies capturés (spec §5.2) : au plus `N` exemples par couple
//! (opération, code), choisis par un réservoir déterministe.
//!
//! - L'opération est (sens, authority, méthode, forme du chemin) : un segment identifiant y
//!   est remplacé par sa nature, comme pour la correspondance du mock (`/users/1` et
//!   `/users/2` ont la même forme).
//! - Chaque échange reçoit une priorité, hachage stable de son identifiant ; le réservoir
//!   garde les `N` plus petites. Chaque échange d'un flux a la même chance d'être gardé, quel
//!   que soit son rang, et l'échantillon ne dépend pas de l'ordre d'arrivée.

use std::collections::BTreeSet;

use crate::infer::path::{SegmentKind, is_identifier};
use crate::model::Direction;

/// Forme d'un chemin : segments littéraux, identifiants remplacés par leur nature.
///
/// Deux chemins ont la même forme si et seulement si leurs segments sont égaux, ou
/// identifiants de même nature, un à un. `\0` ne peut pas figurer dans un chemin d'URI :
/// un segment littéral ne se confond pas avec une nature.
pub fn path_shape(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for s in path.split('/').filter(|s| !s.is_empty()) {
        out.push('/');
        if is_identifier(s) {
            out.push('\0');
            out.push_str(match SegmentKind::of(s) {
                SegmentKind::Integer => "int",
                SegmentKind::Uuid => "uuid",
                SegmentKind::Opaque => "opaque",
                SegmentKind::Word => "word",
            });
        } else {
            out.push_str(s);
        }
    }
    out
}

/// Clé d'échantillonnage d'un échange : (sens, authority, méthode, forme du chemin, code).
pub fn sample_key(
    direction: Direction,
    authority: &str,
    method: &str,
    path: &str,
    status: u16,
) -> String {
    let dir = match direction {
        Direction::Inbound => "in",
        Direction::Outbound => "out",
    };
    format!(
        "{dir}|{authority}|{}|{}|{status}",
        method.to_ascii_uppercase(),
        path_shape(path)
    )
}

/// Priorité d'un échange dans le réservoir : FNV-1a 64 bits de l'identifiant, mélangé par
/// le finaliseur de SplitMix64. Figée : elle ne dépend ni de la plateforme ni de la version
/// de Rust, contrairement au hacheur de la bibliothèque standard.
pub fn priority(id: &str) -> u64 {
    let mut h: u64 = 0xCBF2_9CE4_8422_2325;
    for b in id.bytes() {
        h = (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01B3);
    }
    let mut z = h;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Résultat d'une proposition au réservoir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Offer {
    /// Pas gardé : le réservoir est plein d'exemples de plus petite priorité.
    Rejected,
    /// Gardé dans une place libre.
    Kept,
    /// Gardé à la place de cet exemple, qui perd ses bodies.
    Replaced(String),
}

/// Exemples gardés pour un couple (opération, code) : les plus petites priorités.
#[derive(Debug, Clone, Default)]
pub struct Reservoir {
    kept: BTreeSet<(u64, String)>,
}

impl Reservoir {
    /// Vrai si un échange de cette priorité serait gardé, sans rien modifier.
    pub fn admits(&self, capacity: usize, priority: u64) -> bool {
        if capacity == 0 {
            return false;
        }
        self.kept.len() < capacity || self.kept.last().is_some_and(|(p, _)| priority < *p)
    }

    pub fn offer(&mut self, capacity: usize, priority: u64, id: &str) -> Offer {
        if !self.admits(capacity, priority) {
            return Offer::Rejected;
        }
        self.kept.insert((priority, id.to_string()));
        if self.kept.len() <= capacity {
            return Offer::Kept;
        }
        match self.kept.pop_last() {
            Some((_, out)) => Offer::Replaced(out),
            None => Offer::Kept,
        }
    }

    /// Retire un exemple (évincé du store ou dépouillé de ses bodies) ; faux s'il n'y était pas.
    pub fn remove(&mut self, priority: u64, id: &str) -> bool {
        self.kept.remove(&(priority, id.to_string()))
    }

    pub fn len(&self) -> usize {
        self.kept.len()
    }

    pub fn is_empty(&self) -> bool {
        self.kept.is_empty()
    }

    /// Identifiants gardés, par priorité croissante.
    pub fn ids(&self) -> impl Iterator<Item = &str> {
        self.kept.iter().map(|(_, id)| id.as_str())
    }
}
