//! Path templating (spec §6): segment classes and template rendering.

use std::collections::BTreeSet;

use super::redact::value_rule;

/// One segment of a template.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum Seg {
    Literal(String),
    Param,
}

/// True when the segment is id-like and becomes a parameter (raw bytes, nothing is decoded).
pub fn is_param(segment: &str) -> bool {
    let b = segment.as_bytes();
    if b.is_empty() {
        return false;
    }
    let has_digit = b.iter().any(u8::is_ascii_digit);
    let has_letter = segment.chars().any(char::is_alphabetic);
    b.iter().all(u8::is_ascii_digit)
        || is_uuid(b)
        || is_ulid(b)
        || (b.len() >= 8 && has_digit && b.iter().all(u8::is_ascii_hexdigit))
        || is_rfc3339(b)
        || (segment.chars().count() >= 16 && has_letter && has_digit)
        || value_rule(segment).is_some()
        || b.contains(&b'@')
}

pub(super) fn is_uuid(b: &[u8]) -> bool {
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

fn is_ulid(b: &[u8]) -> bool {
    b.len() == 26
        && b.iter().all(|c| {
            c.is_ascii_alphanumeric()
                && !matches!(c.to_ascii_uppercase(), b'I' | b'L' | b'O' | b'U')
        })
}

fn digits(b: &[u8]) -> Option<u32> {
    if b.is_empty() || !b.iter().all(u8::is_ascii_digit) {
        return None;
    }
    Some(b.iter().fold(0, |n, c| n * 10 + u32::from(c - b'0')))
}

/// `YYYY-MM-DD`, optionally followed by `T` and an RFC 3339 time with an offset.
pub(super) fn is_rfc3339(b: &[u8]) -> bool {
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return false;
    }
    let (Some(_), Some(month), Some(day)) = (digits(&b[..4]), digits(&b[5..7]), digits(&b[8..10]))
    else {
        return false;
    };
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return false;
    }
    let rest = &b[10..];
    if rest.is_empty() {
        return true;
    }
    if !matches!(rest[0], b'T' | b't') || rest.len() < 9 || rest[3] != b':' || rest[6] != b':' {
        return false;
    }
    let (Some(h), Some(m), Some(s)) = (
        digits(&rest[1..3]),
        digits(&rest[4..6]),
        digits(&rest[7..9]),
    ) else {
        return false;
    };
    if h > 23 || m > 59 || s > 60 {
        return false;
    }
    let mut tail = &rest[9..];
    if tail.first() == Some(&b'.') {
        let n = tail[1..].iter().take_while(|c| c.is_ascii_digit()).count();
        if n == 0 {
            return false;
        }
        tail = &tail[1 + n..];
    }
    match tail {
        [b'Z' | b'z'] => true,
        [b'+' | b'-', hh @ .., b':', m1, m2] if hh.len() == 2 => {
            matches!((digits(hh), digits(&[*m1, *m2])), (Some(h), Some(m)) if h < 24 && m < 60)
        }
        _ => false,
    }
}

/// Splits a path (without query) into raw segments; a trailing slash is an empty last segment.
pub fn split_path(path: &str) -> Vec<&str> {
    path.strip_prefix('/').unwrap_or(path).split('/').collect()
}

/// Renders a template, naming parameters `{<literal>_id}` after the previous segment when it
/// is a literal and the name is free, else `{param<n>}` (n = 1-based position).
pub fn render(segs: &[Seg]) -> String {
    let mut taken = BTreeSet::new();
    let mut out = String::new();
    for (i, seg) in segs.iter().enumerate() {
        out.push('/');
        match seg {
            Seg::Literal(l) => out.push_str(l),
            Seg::Param => {
                let named = match i.checked_sub(1).map(|p| &segs[p]) {
                    Some(Seg::Literal(prev)) if !prev.is_empty() => {
                        let base: String = prev
                            .chars()
                            .map(|c| {
                                let c = c.to_ascii_lowercase();
                                if c.is_ascii_lowercase() || c.is_ascii_digit() {
                                    c
                                } else {
                                    '_'
                                }
                            })
                            .collect();
                        Some(format!("{base}_id"))
                    }
                    _ => None,
                };
                let name = match named {
                    Some(n) if !taken.contains(&n) => n,
                    _ => format!("param{}", i + 1),
                };
                out.push('{');
                out.push_str(&name);
                out.push('}');
                taken.insert(name);
            }
        }
    }
    out
}
