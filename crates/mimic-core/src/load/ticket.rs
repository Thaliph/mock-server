//! The load ticket (spec §7.8): `v1.<payload>.<signature>`, both base64url without padding.
//! The payload is the canonical JSON `{"deployment","exp","namespace","uid"}`; the signature
//! (ECDSA P-256 SHA-256, fixed-length `r‖s`) covers [`PREFIX`] followed by the payload bytes.
//! Pure: signing and verifying belong to the caller, which hands the signature primitive in.

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde::{Deserialize, Serialize};

/// What the signed bytes start with: it cannot start a DER certificate (`0x30`), so a ticket
/// signature never certifies anything else.
pub const PREFIX: &[u8] = b"mimic-load-ticket\0";
/// What every ticket starts with.
pub const VERSION: &str = "v1.";
/// Longest ticket the parser looks at, in bytes.
pub const MAX_LEN: usize = 1024;

/// What a ticket says. The field order is the canonical key order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Payload {
    pub deployment: String,
    /// The run's deadline, Unix seconds.
    pub exp: u64,
    pub namespace: String,
    pub uid: String,
}

impl Payload {
    /// The canonical JSON bytes.
    pub fn bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_default()
    }
}

/// The bytes a ticket signature covers for `payload` (its JSON bytes).
pub fn signing_input(payload: &[u8]) -> Vec<u8> {
    let mut input = Vec::with_capacity(PREFIX.len() + payload.len());
    input.extend_from_slice(PREFIX);
    input.extend_from_slice(payload);
    input
}

/// Signs `payload` with `sign`, which gets [`signing_input`] and returns the fixed-length
/// signature.
pub fn issue(payload: &Payload, sign: impl FnOnce(&[u8]) -> Vec<u8>) -> String {
    let bytes = payload.bytes();
    let signature = sign(&signing_input(&bytes));
    format!(
        "{VERSION}{}.{}",
        URL_SAFE_NO_PAD.encode(&bytes),
        URL_SAFE_NO_PAD.encode(signature)
    )
}

/// A ticket taken apart, not yet verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    pub payload: Payload,
    /// The bytes the signature covers.
    pub signed: Vec<u8>,
    pub signature: Vec<u8>,
}

/// Splits `ticket`; `None` when it is not the `v1` form, not base64url, or its payload is not
/// the canonical JSON of a [`Payload`].
pub fn parse(ticket: &str) -> Option<Parsed> {
    if ticket.len() > MAX_LEN {
        return None;
    }
    let rest = ticket.strip_prefix(VERSION)?;
    let (payload, signature) = rest.split_once('.')?;
    let payload = URL_SAFE_NO_PAD.decode(payload).ok()?;
    let signature = URL_SAFE_NO_PAD.decode(signature).ok()?;
    let parsed: Payload = serde_json::from_slice(&payload).ok()?;
    // One byte string per payload: a re-encoded form is another ticket, never accepted.
    if parsed.bytes() != payload {
        return None;
    }
    Some(Parsed {
        payload: parsed,
        signed: signing_input(&payload),
        signature,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn payload() -> Payload {
        Payload {
            deployment: "payment".into(),
            exp: 1_800_000_000,
            namespace: "shop".into(),
            uid: "0b1f6d8a-3c52-4a63-9d0e-6a8f1f6d3c11".into(),
        }
    }

    #[test]
    fn ldt_20_the_payload_is_canonical_json_in_key_order() {
        assert_eq!(
            String::from_utf8(payload().bytes()).unwrap(),
            r#"{"deployment":"payment","exp":1800000000,"namespace":"shop","uid":"0b1f6d8a-3c52-4a63-9d0e-6a8f1f6d3c11"}"#
        );
    }

    #[test]
    fn ldt_20_the_signed_bytes_start_with_the_domain_prefix() {
        let mut seen = Vec::new();
        let ticket = issue(&payload(), |input| {
            seen = input.to_vec();
            vec![7; 64]
        });
        assert!(seen.starts_with(b"mimic-load-ticket\0{"));
        let parsed = parse(&ticket).unwrap();
        assert_eq!(parsed.signed, seen);
        assert_eq!(parsed.signature, vec![7; 64]);
        assert_eq!(parsed.payload, payload());
    }

    #[test]
    fn ldt_20_a_ticket_without_the_prefix_or_with_a_changed_form_does_not_parse() {
        let ticket = issue(&payload(), |_| vec![1; 64]);
        assert!(parse(&ticket).is_some());
        assert!(parse(&ticket.replacen("v1.", "v2.", 1)).is_none());
        assert!(parse(ticket.strip_prefix("v1.").unwrap()).is_none());
        assert!(parse("v1.e30.AA").is_none());
        assert!(parse("v1.").is_none());
        assert!(parse(&format!("{ticket}.x")).is_none());
        assert!(parse(&"v1.".repeat(400)).is_none());
        // The same fields in another key order are another byte string.
        let reordered = format!(
            "v1.{}.AA",
            URL_SAFE_NO_PAD.encode(br#"{"exp":1,"deployment":"a","namespace":"b","uid":"c"}"#)
        );
        assert!(parse(&reordered).is_none());
    }
}
