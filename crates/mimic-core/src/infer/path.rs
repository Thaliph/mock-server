//! Templating des chemins : `/users/42` et `/users/43` → `/users/{userId}` (spec §6.2).
//!
//! Les chemins d'une même authority sont rangés dans un arbre préfixe, toutes méthodes
//! confondues (`GET /carts/1` et `DELETE /carts/2` partagent le template). À chaque noeud,
//! les enfants « variables » sont fusionnés en un paramètre selon ces règles :
//!
//! - un segment qui ressemble à un identifiant (entier, UUID, ULID, hex, base64url, date)
//!   est toujours variable ;
//! - un segment fréquent (plus de [`FREQUENT_SHARE`] des passages par le noeud) qui ne
//!   ressemble pas à un identifiant reste littéral (`/users/me`) ;
//! - les autres segments sont variables s'ils ressemblent à un slug (chiffre, `-`, `_`, `.`) ;
//!   un mot purement alphabétique ne l'est qu'au-delà de [`WORD_MIN_DISTINCT`] valeurs ;
//! - la fusion n'a lieu que si les valeurs variables distinctes sont au moins [`MIN_DISTINCT`].

use std::collections::{BTreeMap, BTreeSet};

/// Nombre minimal de valeurs distinctes pour créer un paramètre.
pub const MIN_DISTINCT: usize = 5;
/// Nombre minimal de mots alphabétiques distincts pour en faire un paramètre.
pub const WORD_MIN_DISTINCT: usize = 20;
/// Au-delà de cette part des passages, un segment non identifiant reste littéral.
pub const FREQUENT_SHARE: f64 = 0.10;

/// Forme d'un segment, utilisée pour typer le paramètre.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum SegmentKind {
    Integer,
    Uuid,
    /// Autre identifiant (ULID, hex, base64url, date) ou slug.
    Opaque,
    /// Mot alphabétique.
    Word,
}

impl SegmentKind {
    pub fn of(s: &str) -> Self {
        if !s.is_empty() && s.len() <= 18 && s.bytes().all(|b| b.is_ascii_digit()) {
            SegmentKind::Integer
        } else if is_uuid(s) {
            SegmentKind::Uuid
        } else if s.bytes().all(|b| b.is_ascii_alphabetic()) {
            SegmentKind::Word
        } else {
            SegmentKind::Opaque
        }
    }

    fn is_identifier(self, s: &str) -> bool {
        match self {
            SegmentKind::Integer | SegmentKind::Uuid => true,
            SegmentKind::Word => false,
            SegmentKind::Opaque => is_ulid(s) || is_hex_id(s) || is_b64_id(s) || is_iso_date(s),
        }
    }
}

/// Description d'un paramètre de chemin d'un template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathParam {
    pub name: String,
    /// Formes observées pour ce paramètre.
    pub kinds: BTreeSet<SegmentKind>,
}

/// Résultat du templating d'un chemin concret.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Templated {
    pub template: String,
    pub params: Vec<PathParam>,
    /// Valeur concrète de chaque paramètre, dans l'ordre de `params`.
    pub values: Vec<String>,
}

#[derive(Debug, Default)]
struct Node {
    count: u64,
    children: BTreeMap<String, Node>,
    param: Option<Box<Node>>,
    kinds: BTreeSet<SegmentKind>,
}

impl Node {
    fn insert(&mut self, segments: &[&str]) {
        self.count += 1;
        if let Some((first, rest)) = segments.split_first() {
            self.children
                .entry((*first).to_string())
                .or_default()
                .insert(rest);
        }
    }

    fn merge(&mut self, other: Node) {
        self.count += other.count;
        self.kinds.extend(other.kinds);
        for (k, child) in other.children {
            self.children.entry(k).or_default().merge(child);
        }
        if let Some(p) = other.param {
            match &mut self.param {
                Some(mine) => mine.merge(*p),
                None => self.param = Some(p),
            }
        }
    }

    /// Fusionne récursivement les enfants variables en paramètre.
    fn resolve(&mut self) {
        let passages: u64 = self.children.values().map(|c| c.count).sum();
        let mut variable = vec![];
        for (seg, child) in &self.children {
            let kind = SegmentKind::of(seg);
            let ident = kind.is_identifier(seg);
            let share = child.count as f64 / passages.max(1) as f64;
            if ident || share <= FREQUENT_SHARE {
                variable.push((seg.clone(), kind));
            }
        }
        let words = variable
            .iter()
            .filter(|(_, k)| *k == SegmentKind::Word)
            .count();
        if words < WORD_MIN_DISTINCT {
            variable.retain(|(_, k)| *k != SegmentKind::Word);
        }
        if variable.len() >= MIN_DISTINCT {
            let mut param = self.param.take().map(|b| *b).unwrap_or_default();
            for (seg, kind) in variable {
                if let Some(child) = self.children.remove(&seg) {
                    param.kinds.insert(kind);
                    param.merge(child);
                }
            }
            self.param = Some(Box::new(param));
        }
        for child in self.children.values_mut() {
            child.resolve();
        }
        if let Some(p) = &mut self.param {
            p.resolve();
        }
    }
}

/// Arbre de templating construit sur un ensemble de chemins.
#[derive(Debug, Default)]
pub struct PathTemplater {
    root: Node,
}

impl PathTemplater {
    pub fn new<'a>(paths: impl IntoIterator<Item = &'a str>) -> Self {
        let mut root = Node::default();
        for p in paths {
            root.insert(&split(p));
        }
        root.resolve();
        Self { root }
    }

    /// Template d'un chemin (qui doit faire partie de l'ensemble initial).
    pub fn template(&self, path: &str) -> Templated {
        let mut node = &self.root;
        let mut parts = vec![];
        let mut params: Vec<PathParam> = vec![];
        let mut values = vec![];
        let mut prev_literal: Option<&str> = None;
        let segments = split(path);
        for seg in &segments {
            if let Some(child) = node.children.get(*seg) {
                parts.push((*seg).to_string());
                prev_literal = Some(seg);
                node = child;
            } else if let Some(p) = &node.param {
                let ordinal = params.len() + 1;
                let mut name = match prev_literal {
                    Some(parent) => format!("{}Id", singular(parent)),
                    None => format!("param{ordinal}"),
                };
                if params.iter().any(|q| q.name == name) {
                    name = format!("{name}{ordinal}");
                }
                parts.push(format!("{{{name}}}"));
                params.push(PathParam {
                    name,
                    kinds: p.kinds.clone(),
                });
                values.push((*seg).to_string());
                prev_literal = None;
                node = p;
            } else {
                // Chemin inconnu de l'arbre : on le garde littéral.
                parts.push((*seg).to_string());
                prev_literal = Some(seg);
            }
        }
        Templated {
            template: format!("/{}", parts.join("/")),
            params,
            values,
        }
    }
}

fn split(path: &str) -> Vec<&str> {
    path.split('/').filter(|s| !s.is_empty()).collect()
}

/// Singulier naïf d'un segment, en camelCase : `categories` → `category`, `line-items` → `lineItem`.
fn singular(word: &str) -> String {
    let camel: String = word
        .split(['-', '_', '.'])
        .filter(|s| !s.is_empty())
        .enumerate()
        .map(|(i, part)| {
            let lower = part.to_ascii_lowercase();
            if i == 0 {
                lower
            } else {
                let mut c = lower.chars();
                c.next()
                    .map(|f| f.to_ascii_uppercase().to_string() + c.as_str())
                    .unwrap_or_default()
            }
        })
        .collect();
    let camel = if camel.is_empty() {
        "param".into()
    } else {
        camel
    };
    if let Some(stem) = camel.strip_suffix("ies") {
        format!("{stem}y")
    } else if camel.ends_with("ss") {
        camel
    } else if let Some(stem) = camel.strip_suffix('s') {
        stem.to_string()
    } else {
        camel
    }
}

pub(crate) fn is_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

fn is_ulid(s: &str) -> bool {
    s.len() == 26
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b.is_ascii_uppercase() && !b"ILOU".contains(&b)))
}

fn is_hex_id(s: &str) -> bool {
    s.len() >= 8
        && s.bytes().all(|b| b.is_ascii_hexdigit())
        && s.bytes().any(|b| b.is_ascii_digit())
}

fn is_b64_id(s: &str) -> bool {
    s.len() >= 16
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        && s.bytes().any(|b| b.is_ascii_digit())
        && s.bytes().any(|b| b.is_ascii_alphabetic())
}

pub(crate) fn is_iso_date(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 10
        && b.iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            _ => c.is_ascii_digit(),
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(SegmentKind::of("42"), SegmentKind::Integer);
        assert_eq!(
            SegmentKind::of("0b9c3c2e-4d4c-4a52-9a43-3f1a8a6e6b10"),
            SegmentKind::Uuid
        );
        assert_eq!(SegmentKind::of("users"), SegmentKind::Word);
        assert_eq!(SegmentKind::of("my-post-1"), SegmentKind::Opaque);
        assert!(SegmentKind::Opaque.is_identifier("01HZX3K5V8QWJ6Y7N2M4P9R0ST"));
        assert!(SegmentKind::Opaque.is_identifier("2026-10-17"));
        assert!(!SegmentKind::Opaque.is_identifier("v1"));
    }

    #[test]
    fn singulars() {
        assert_eq!(singular("users"), "user");
        assert_eq!(singular("categories"), "category");
        assert_eq!(singular("line-items"), "lineItem");
        assert_eq!(singular("address"), "address");
    }

    #[test]
    fn slugs_are_merged_when_rare() {
        let paths: Vec<String> = (0..12).map(|i| format!("/posts/post-{i}")).collect();
        let t = PathTemplater::new(paths.iter().map(String::as_str));
        assert_eq!(t.template("/posts/post-3").template, "/posts/{postId}");
    }

    #[test]
    fn many_words_end_up_merged() {
        let names: Vec<String> = (0..25)
            .map(|i| {
                format!(
                    "/u/{}",
                    "abcdefghijklmnopqrstuvwxyz"
                        .get(i..i + 1)
                        .unwrap()
                        .repeat(3)
                )
            })
            .collect();
        let t = PathTemplater::new(names.iter().map(String::as_str));
        assert_eq!(t.template("/u/aaa").template, "/u/{uId}");
    }

    #[test]
    fn root_and_trailing_slash() {
        let t = PathTemplater::new(["/", "/a/"]);
        assert_eq!(t.template("/").template, "/");
        assert_eq!(t.template("/a/").template, "/a");
    }
}
