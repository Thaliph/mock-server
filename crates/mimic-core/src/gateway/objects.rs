//! The objects of an accepted Gateway (spec §7.10 "Generated objects").

use sha2::{Digest, Sha256};

use super::MAX_NAME;

/// Characters of the Gateway name kept in the hashed form.
const HASHED_PREFIX: usize = 46;
/// Hex digits of the SHA-256 in the hashed form.
const HASH_DIGITS: usize = 10;

/// The name of the Deployment, Service and PodDisruptionBudget of the Gateway `gateway`
/// (GW-01): `<gateway>-mimic` when that is a DNS-1035 label of at most 63 characters
/// (GEP-1762), else a cut of the name, `-`, 10 hex digits of its SHA-256 and `-mimic`.
pub fn name(gateway: &str) -> String {
    let plain = format!("{gateway}-mimic");
    if plain.len() <= MAX_NAME && dns_1035(&plain) {
        return plain;
    }
    let mut cut = String::new();
    if !gateway.starts_with(|c: char| c.is_ascii_lowercase()) {
        cut.push('g');
    }
    cut.extend(gateway.chars().map(|c| if c == '.' { '-' } else { c }));
    let mut out: String = cut.chars().take(HASHED_PREFIX).collect();
    out.truncate(out.trim_end_matches('-').len());
    out.push('-');
    out.push_str(&hex(&Sha256::digest(gateway.as_bytes()), HASH_DIGITS));
    out.push_str("-mimic");
    out
}

/// `[a-z]([-a-z0-9]*[a-z0-9])?`.
fn dns_1035(s: &str) -> bool {
    let b = s.as_bytes();
    let inner = |c: &u8| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-';
    match (b.first(), b.last()) {
        (Some(first), Some(last)) => {
            first.is_ascii_lowercase() && *last != b'-' && b.iter().all(inner)
        }
        _ => false,
    }
}

/// The first `digits` lower-case hex digits of `bytes`.
fn hex(bytes: &[u8], digits: usize) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    bytes
        .iter()
        .flat_map(|b| [HEX[usize::from(b >> 4)], HEX[usize::from(b & 0xf)]])
        .take(digits)
        .map(char::from)
        .collect()
}
