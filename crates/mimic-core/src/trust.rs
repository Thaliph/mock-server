//! PEM certificate bundles for `mimic-trust` (spec §7.1): parsing, deduplication in first-seen
//! order and re-encoding. Pure: reading files and writing the PKCS12 store is the binary's job.

use std::collections::BTreeSet;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;

const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
const END: &str = "-----END CERTIFICATE-----";

/// Certificates found in a PEM text.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parsed {
    /// DER certificates, in file order (duplicates kept).
    pub certs: Vec<Vec<u8>>,
    /// `CERTIFICATE` blocks that are unterminated, not base64, or rejected by the validator.
    pub skipped: usize,
}

/// Extracts the `CERTIFICATE` blocks of `text`; text outside the blocks (comments, other PEM
/// types) is ignored. `is_cert` tells whether decoded bytes are a certificate.
pub fn parse_pem(text: &str, is_cert: impl Fn(&[u8]) -> bool) -> Parsed {
    let mut parsed = Parsed::default();
    // The base64 body while inside a CERTIFICATE block.
    let mut body: Option<String> = None;
    for line in text.lines() {
        let line = line.trim();
        if line == BEGIN {
            if body.is_some() {
                parsed.skipped += 1;
            }
            body = Some(String::new());
        } else if line == END {
            if let Some(b) = body.take() {
                match STANDARD.decode(b.as_bytes()) {
                    Ok(der) if is_cert(&der) => parsed.certs.push(der),
                    _ => parsed.skipped += 1,
                }
            }
        } else if let Some(b) = body.as_mut() {
            b.push_str(line);
        }
    }
    if body.is_some() {
        parsed.skipped += 1;
    }
    parsed
}

/// Concatenates the lists, dropping every certificate already seen (first one wins).
pub fn merge(lists: &[&[Vec<u8>]]) -> Vec<Vec<u8>> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    for cert in lists.iter().flat_map(|l| l.iter()) {
        if seen.insert(cert.as_slice()) {
            out.push(cert.clone());
        }
    }
    out
}

/// One `CERTIFICATE` block per certificate, base64 wrapped at 64 columns, `\n` line ends.
pub fn encode_pem(certs: &[Vec<u8>]) -> String {
    let mut out = String::new();
    for cert in certs {
        out.push_str(BEGIN);
        out.push('\n');
        let encoded = STANDARD.encode(cert);
        for chunk in encoded.as_bytes().chunks(64) {
            // base64 output is ASCII, so every chunk is valid UTF-8.
            out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
            out.push('\n');
        }
        out.push_str(END);
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn block(der: &[u8]) -> String {
        encode_pem(&[der.to_vec()])
    }
    fn any(_: &[u8]) -> bool {
        true
    }

    #[test]
    fn tls_13_encoding_round_trips_and_wraps_at_64_columns() {
        let der: Vec<u8> = (0..200u8).collect();
        let pem = block(&der);
        assert!(pem.starts_with("-----BEGIN CERTIFICATE-----\n"));
        assert!(pem.ends_with("-----END CERTIFICATE-----\n"));
        assert!(pem.lines().all(|l| l.len() <= 64 || l.starts_with("-----")));
        assert_eq!(
            parse_pem(&pem, any),
            Parsed {
                certs: vec![der],
                skipped: 0
            }
        );
    }

    #[test]
    fn tls_13_text_and_other_pem_types_are_ignored() {
        let text = format!(
            "# Debian bundle\nsome noise\n{}-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n{}",
            block(b"one"),
            block(b"two")
        );
        let parsed = parse_pem(&text, any);
        assert_eq!(parsed.certs, vec![b"one".to_vec(), b"two".to_vec()]);
        assert_eq!(parsed.skipped, 0);
    }

    #[test]
    fn tls_13_unparsable_blocks_are_skipped_and_counted() {
        let text = format!(
            "-----BEGIN CERTIFICATE-----\n!!not base64!!\n-----END CERTIFICATE-----\n{}{}-----BEGIN CERTIFICATE-----\nAAAA\n",
            block(b"good"),
            block(b"bad")
        );
        let parsed = parse_pem(&text, |der| der == b"good");
        assert_eq!(parsed.certs, vec![b"good".to_vec()]);
        assert_eq!(parsed.skipped, 3);
    }

    #[test]
    fn tls_13_crlf_line_ends_are_accepted() {
        let text = block(b"crlf").replace('\n', "\r\n");
        assert_eq!(parse_pem(&text, any).certs, vec![b"crlf".to_vec()]);
    }

    #[test]
    fn tls_13_merge_drops_duplicates_keeping_first_seen_order() {
        let a = vec![b"1".to_vec(), b"2".to_vec(), b"1".to_vec()];
        let b = vec![b"3".to_vec(), b"2".to_vec()];
        assert_eq!(
            merge(&[&a, &b]),
            vec![b"1".to_vec(), b"2".to_vec(), b"3".to_vec()]
        );
        assert!(merge(&[]).is_empty());
    }
}
