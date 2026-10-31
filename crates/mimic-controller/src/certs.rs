//! Certificates of the webhook (spec §7.1): the decision is a pure function of what the Secret
//! holds and the current time; issuing needs nothing but randomness.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rcgen::{
    BasicConstraints, CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use time::OffsetDateTime;
use x509_parser::extensions::GeneralName;
use x509_parser::pem::parse_x509_pem;

#[cfg(test)]
const DAY: Duration = Duration::from_secs(24 * 3600);
const CA_LIFETIME: Duration = Duration::from_secs(10 * 365 * 24 * 3600);
const SERVING_LIFETIME: Duration = Duration::from_secs(365 * 24 * 3600);
/// A serving certificate with less than this left is renewed.
const RENEW_BEFORE: Duration = Duration::from_secs(30 * 24 * 3600);
/// Certificates are valid from slightly before issuance, against clock skew.
const SKEW: Duration = Duration::from_secs(3600);

/// The PEM material of the Secret; every part may be empty.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct Stored {
    pub ca_crt: Vec<u8>,
    pub ca_key: Vec<u8>,
    pub tls_crt: Vec<u8>,
    pub tls_key: Vec<u8>,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Decision {
    /// New CA, hence a new serving certificate.
    NewCa,
    /// New serving certificate signed by the existing CA.
    NewServing,
    Keep,
}

/// What to do with `stored` for a serving certificate valid for `dns_name`.
pub fn decide(stored: &Stored, dns_name: &str, now: SystemTime) -> Decision {
    let ca_usable = KeyPair::from_pem(&text(&stored.ca_key)).is_ok()
        && not_after(&stored.ca_crt).is_some_and(|end| end > now);
    if !ca_usable {
        return Decision::NewCa;
    }
    let key_ok = KeyPair::from_pem(&text(&stored.tls_key)).is_ok();
    let fresh = not_after(&stored.tls_crt).is_some_and(|end| end > now + RENEW_BEFORE);
    if key_ok && fresh && has_dns_name(&stored.tls_crt, dns_name) {
        Decision::Keep
    } else {
        Decision::NewServing
    }
}

/// Applies `decision` to `stored`. The CA is untouched unless `decision` is `NewCa`.
pub fn issue(
    stored: &Stored,
    decision: Decision,
    dns_name: &str,
    now: SystemTime,
) -> Result<Stored, String> {
    let mut next = stored.clone();
    if decision == Decision::NewCa {
        let key = KeyPair::generate().map_err(|e| format!("CA key: {e}"))?;
        let mut params = CertificateParams::default();
        params
            .distinguished_name
            .push(DnType::CommonName, "mimic-webhook-ca");
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        validity(&mut params, now, CA_LIFETIME);
        let cert = params
            .self_signed(&key)
            .map_err(|e| format!("CA certificate: {e}"))?;
        next.ca_crt = cert.pem().into_bytes();
        next.ca_key = key.serialize_pem().into_bytes();
    }
    if decision != Decision::Keep {
        let ca_key = KeyPair::from_pem(&text(&next.ca_key)).map_err(|e| format!("CA key: {e}"))?;
        let issuer = Issuer::from_ca_cert_pem(&text(&next.ca_crt), ca_key)
            .map_err(|e| format!("CA certificate: {e}"))?;
        let key = KeyPair::generate().map_err(|e| format!("serving key: {e}"))?;
        let mut params = CertificateParams::new(vec![dns_name.to_string()])
            .map_err(|e| format!("serving certificate: {e}"))?;
        params
            .distinguished_name
            .push(DnType::CommonName, dns_name.to_string());
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        validity(&mut params, now, SERVING_LIFETIME);
        let cert = params
            .signed_by(&key, &issuer)
            .map_err(|e| format!("serving certificate: {e}"))?;
        next.tls_crt = cert.pem().into_bytes();
        next.tls_key = key.serialize_pem().into_bytes();
    }
    Ok(next)
}

fn validity(params: &mut CertificateParams, now: SystemTime, lifetime: Duration) {
    params.not_before = datetime(now - SKEW);
    params.not_after = datetime(now + lifetime);
}

fn datetime(at: SystemTime) -> OffsetDateTime {
    let seconds = at.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
    OffsetDateTime::from_unix_timestamp(i64::try_from(seconds).unwrap_or(i64::MAX))
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

fn text(pem: &[u8]) -> String {
    String::from_utf8_lossy(pem).into_owned()
}

/// End of validity of the first certificate of `pem`.
pub fn not_after(pem: &[u8]) -> Option<SystemTime> {
    let (_, pem) = parse_x509_pem(pem).ok()?;
    let cert = pem.parse_x509().ok()?;
    let end = u64::try_from(cert.validity().not_after.timestamp()).ok()?;
    Some(UNIX_EPOCH + Duration::from_secs(end))
}

fn has_dns_name(pem: &[u8], dns_name: &str) -> bool {
    let Ok((_, pem)) = parse_x509_pem(pem) else {
        return false;
    };
    let Ok(cert) = pem.parse_x509() else {
        return false;
    };
    match cert.subject_alternative_name() {
        Ok(Some(san)) => san
            .value
            .general_names
            .iter()
            .any(|name| matches!(name, GeneralName::DNSName(n) if *n == dns_name)),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const DNS: &str = "mimic.mimic-system.svc";

    fn at(days: u64) -> SystemTime {
        UNIX_EPOCH + DAY * (20_000 + days as u32)
    }

    fn full(now: SystemTime) -> Stored {
        issue(&Stored::default(), Decision::NewCa, DNS, now).unwrap()
    }

    #[test]
    fn inj_10_no_or_unparsable_ca_gives_a_new_ca() {
        let now = at(0);
        assert_eq!(decide(&Stored::default(), DNS, now), Decision::NewCa);
        let good = full(now);
        for broken in [
            Stored {
                ca_crt: b"junk".to_vec(),
                ..good.clone()
            },
            Stored {
                ca_key: b"junk".to_vec(),
                ..good.clone()
            },
            Stored {
                ca_crt: Vec::new(),
                ..good.clone()
            },
            Stored {
                ca_key: Vec::new(),
                ..good.clone()
            },
        ] {
            assert_eq!(decide(&broken, DNS, now), Decision::NewCa);
        }
        // A CA that is over is as good as none.
        assert_eq!(decide(&good, DNS, at(3651)), Decision::NewCa);
    }

    #[test]
    fn inj_10_missing_unparsable_or_wrong_serving_certificate_is_replaced() {
        let now = at(0);
        let good = full(now);
        assert_eq!(decide(&good, DNS, now), Decision::Keep);
        for broken in [
            Stored {
                tls_crt: Vec::new(),
                ..good.clone()
            },
            Stored {
                tls_crt: b"junk".to_vec(),
                ..good.clone()
            },
            Stored {
                tls_key: Vec::new(),
                ..good.clone()
            },
            Stored {
                tls_key: b"junk".to_vec(),
                ..good.clone()
            },
        ] {
            assert_eq!(decide(&broken, DNS, now), Decision::NewServing);
        }
        assert_eq!(
            decide(&good, "other.mimic-system.svc", now),
            Decision::NewServing
        );
    }

    #[test]
    fn inj_10_serving_certificate_is_renewed_within_30_days_of_expiry() {
        let good = full(at(0));
        // Valid for 365 days from at(0).
        assert_eq!(decide(&good, DNS, at(334)), Decision::Keep);
        assert_eq!(decide(&good, DNS, at(336)), Decision::NewServing);
        assert_eq!(decide(&good, DNS, at(366)), Decision::NewServing);
    }

    #[test]
    fn inj_10_renewal_keeps_the_ca_and_a_new_ca_replaces_everything() {
        let first = full(at(0));
        let renewed = issue(&first, Decision::NewServing, DNS, at(340)).unwrap();
        assert_eq!(renewed.ca_crt, first.ca_crt);
        assert_eq!(renewed.ca_key, first.ca_key);
        assert_ne!(renewed.tls_crt, first.tls_crt);
        assert_eq!(decide(&renewed, DNS, at(340)), Decision::Keep);

        let again = issue(&first, Decision::NewCa, DNS, at(1)).unwrap();
        assert_ne!(again.ca_crt, first.ca_crt);
        assert_ne!(again.tls_crt, first.tls_crt);
    }

    #[test]
    fn inj_10_serving_certificate_is_signed_by_the_ca_for_the_service_name() {
        let stored = full(at(0));
        let (_, ca) = parse_x509_pem(&stored.ca_crt).unwrap();
        let ca = ca.parse_x509().unwrap();
        let (_, leaf) = parse_x509_pem(&stored.tls_crt).unwrap();
        let leaf = leaf.parse_x509().unwrap();
        leaf.verify_signature(Some(ca.public_key())).unwrap();
        assert!(has_dns_name(&stored.tls_crt, DNS));
        let ca_end = not_after(&stored.ca_crt).unwrap();
        assert!(ca_end > at(3600) && ca_end < at(3700));
    }
}
