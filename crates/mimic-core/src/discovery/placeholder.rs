//! Placeholders (spec §6): what a redacted value becomes. One constant per type, chosen by the
//! rule that fired and never by the value, so no byte, length or hash of a secret survives.
//! The legacy marker `<redacted>` of up to F12 is read, and upgraded, never written (but for
//! the host of a redacted outbound authority).

use super::redact::MARKER;
use super::report::Report;

/// The type of a redacted value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Placeholder {
    Auth,
    Token,
    Jwt,
    ApiKey,
    Password,
    PrivateKey,
    Secret,
    Session,
    Cookie,
    Signature,
    Card,
    Iban,
    Ssn,
}

/// The 13 placeholders.
pub const PLACEHOLDERS: [&str; 13] = [
    "mimic-auth",
    "mimic-token",
    "mimic-jwt",
    "mimic-api-key",
    "mimic-password",
    "mimic-private-key",
    "mimic-secret",
    "mimic-session",
    "mimic-cookie",
    "mimic-signature",
    "mimic-card",
    "mimic-iban",
    "mimic-ssn",
];

impl Placeholder {
    pub const fn as_str(self) -> &'static str {
        match self {
            Placeholder::Auth => PLACEHOLDERS[0],
            Placeholder::Token => PLACEHOLDERS[1],
            Placeholder::Jwt => PLACEHOLDERS[2],
            Placeholder::ApiKey => PLACEHOLDERS[3],
            Placeholder::Password => PLACEHOLDERS[4],
            Placeholder::PrivateKey => PLACEHOLDERS[5],
            Placeholder::Secret => PLACEHOLDERS[6],
            Placeholder::Session => PLACEHOLDERS[7],
            Placeholder::Cookie => PLACEHOLDERS[8],
            Placeholder::Signature => PLACEHOLDERS[9],
            Placeholder::Card => PLACEHOLDERS[10],
            Placeholder::Iban => PLACEHOLDERS[11],
            Placeholder::Ssn => PLACEHOLDERS[12],
        }
    }
}

impl std::fmt::Display for Placeholder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

fn word(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_')
}

/// True when `value` holds a placeholder as a word of its own (`Bearer mimic-token`,
/// `a=mimic-cookie`, `/reset/mimic-jwt`), or the legacy marker.
pub fn is_marked(value: &str) -> bool {
    if value.contains(MARKER) {
        return true;
    }
    let bytes = value.as_bytes();
    value.match_indices("mimic-").any(|(at, _)| {
        let rest = &value[at..];
        PLACEHOLDERS.iter().any(|p| {
            rest.starts_with(p)
                && (at == 0 || !word(bytes[at - 1]))
                && bytes.get(at + p.len()).is_none_or(|b| !word(*b))
        })
    })
}

/// A legacy marker in `value` upgraded when no name gave it a type: after `Bearer` and
/// `Basic` the credential of the scheme, anywhere else a secret.
pub fn upgrade_text(value: &str) -> String {
    if !value.contains(MARKER) {
        return value.to_string();
    }
    value
        .replace(
            &format!("Bearer {MARKER}"),
            &format!("Bearer {}", Placeholder::Token),
        )
        .replace(
            &format!("Basic {MARKER}"),
            &format!("Basic {}", Placeholder::Auth),
        )
        .replace(MARKER, Placeholder::Secret.as_str())
}

/// Upgrades a report written up to F12 (spec §6, placeholder upgrade): each legacy marker in
/// a sample or a tree key becomes the placeholder its field gives. Pure and idempotent; a
/// report without a legacy marker is left as it is, byte for byte. Authority hosts keep the
/// marker.
pub fn upgrade(report: &mut Report) {
    report.upgrade_placeholders();
}
