//! TLS seed and per-deployment root CAs (spec §5, §10.2): the seed is the only secret; a root is
//! a pure function of the seed and `<namespace>/<deployment>`, computed in memory when needed
//! and never stored. Nothing in this module logs or prints key material.

use std::collections::BTreeMap;
use std::time::SystemTime;

use k8s_openapi::ByteString;
use zeroize::Zeroizing;

/// The random secret every root derives from, and the time it was created at.
pub struct Seed {
    bytes: Zeroizing<[u8; 32]>,
    created: SystemTime,
}

impl std::fmt::Debug for Seed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Seed")
            .field("created", &self.created)
            .finish_non_exhaustive()
    }
}

impl Seed {
    /// A new random seed created at `now`.
    pub fn generate(_now: SystemTime) -> Seed {
        unimplemented!()
    }

    /// The seed held by a Secret, if it holds a valid one: 32 bytes under `seed`, a decimal
    /// number of seconds since the Unix epoch under `created`.
    pub fn from_data(_data: Option<&BTreeMap<String, ByteString>>) -> Option<Seed> {
        unimplemented!()
    }

    pub fn to_data(&self) -> BTreeMap<String, ByteString> {
        unimplemented!()
    }

    pub fn created(&self) -> SystemTime {
        self.created
    }
}

/// The root CA of one deployment.
pub struct Root;

impl Root {
    pub fn certificate_pem(&self) -> &str {
        unimplemented!()
    }

    /// SubjectPublicKeyInfo (DER) of the root key.
    pub fn public_key_der(&self) -> Vec<u8> {
        unimplemented!()
    }

    /// Certifies the P-256 key `spki_der` (SubjectPublicKeyInfo, DER) as a path-length-0
    /// intermediate valid from `now - 1 h` for 25 h. Returns the certificate as PEM.
    pub fn sign_intermediate(&self, _spki_der: &[u8], _now: SystemTime) -> Result<String, String> {
        unimplemented!()
    }
}

/// The root of `<namespace>/<deployment>` under `seed`.
pub fn derive_root(_seed: &Seed, _namespace: &str, _deployment: &str) -> Result<Root, String> {
    unimplemented!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{KeyPair, PublicKeyData};
    use std::time::{Duration, UNIX_EPOCH};
    use x509_parser::pem::parse_x509_pem;

    const HOUR: Duration = Duration::from_secs(3600);

    fn at(hours: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(1_800_000_000) + HOUR * hours as u32
    }

    fn seed(byte: u8) -> Seed {
        Seed {
            bytes: Zeroizing::new([byte; 32]),
            created: at(0),
        }
    }

    fn parse(pem: &str) -> Vec<u8> {
        parse_x509_pem(pem.as_bytes()).unwrap().1.contents
    }

    fn subject(root: &Root) -> String {
        let der = parse(root.certificate_pem());
        let (_, cert) = x509_parser::parse_x509_certificate(&der).unwrap();
        cert.subject().to_string()
    }

    #[test]
    fn tls_07_same_inputs_same_root_other_inputs_other_root() {
        let a = derive_root(&seed(1), "shop", "cart").unwrap();
        let again = derive_root(&seed(1), "shop", "cart").unwrap();
        assert_eq!(a.public_key_der(), again.public_key_der());
        assert_eq!(subject(&a), subject(&again));
        assert!(subject(&a).contains("CN=mimic shop/cart"), "{}", subject(&a));
        assert!(subject(&a).contains("O=mimic"));
        for (s, ns, dep) in [(2, "shop", "cart"), (1, "other", "cart"), (1, "shop", "pay")] {
            let b = derive_root(&seed(s), ns, dep).unwrap();
            assert_ne!(a.public_key_der(), b.public_key_der(), "{s} {ns} {dep}");
        }
        // The separator is part of the input: "a/b" + "c" is not "a" + "b/c".
        let x = derive_root(&seed(1), "a/b", "c").unwrap();
        let y = derive_root(&seed(1), "a", "b/c").unwrap();
        assert_ne!(x.public_key_der(), y.public_key_der());
    }

    #[test]
    fn tls_07_root_is_a_p256_ca_valid_from_seed_creation_for_10_years() {
        let root = derive_root(&seed(1), "shop", "cart").unwrap();
        let der = parse(root.certificate_pem());
        let (_, cert) = x509_parser::parse_x509_certificate(&der).unwrap();
        assert!(cert.basic_constraints().unwrap().unwrap().value.ca);
        assert!(cert.key_usage().unwrap().unwrap().value.key_cert_sign());
        cert.verify_signature(None).unwrap();
        let curve = cert.public_key().algorithm.parameters.as_ref().unwrap();
        assert_eq!(curve.as_oid().unwrap().to_string(), "1.2.840.10045.3.1.7");
        let created = at(0).duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        assert_eq!(cert.validity().not_before.timestamp(), created - 3600);
        assert_eq!(
            cert.validity().not_after.timestamp() - created,
            10 * 365 * 24 * 3600
        );
    }

    #[test]
    fn tls_07_intermediate_chains_to_the_root_for_25_hours() {
        let root = derive_root(&seed(1), "shop", "cart").unwrap();
        let key = KeyPair::generate().unwrap();
        let pem = root
            .sign_intermediate(&key.subject_public_key_info(), at(100))
            .unwrap();
        let der = parse(&pem);
        let (_, inter) = x509_parser::parse_x509_certificate(&der).unwrap();
        let root_der = parse(root.certificate_pem());
        let (_, root_cert) = x509_parser::parse_x509_certificate(&root_der).unwrap();
        inter
            .verify_signature(Some(root_cert.public_key()))
            .unwrap();
        assert_eq!(inter.issuer().to_string(), root_cert.subject().to_string());
        let bc = inter.basic_constraints().unwrap().unwrap().value;
        assert!(bc.ca);
        assert_eq!(bc.path_len_constraint, Some(0));
        assert!(inter.key_usage().unwrap().unwrap().value.key_cert_sign());
        let issued = at(100).duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
        assert_eq!(inter.validity().not_before.timestamp(), issued - 3600);
        assert_eq!(inter.validity().not_after.timestamp(), issued + 25 * 3600);
        // The certificate carries the sidecar's key, not the root's.
        assert_eq!(inter.public_key().raw, &key.subject_public_key_info()[..]);
    }

    #[test]
    fn tls_07_only_p256_keys_are_certified() {
        let root = derive_root(&seed(1), "shop", "cart").unwrap();
        assert!(root.sign_intermediate(b"junk", at(0)).is_err());
        let p384 = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P384_SHA384).unwrap();
        assert!(
            root.sign_intermediate(&p384.subject_public_key_info(), at(0))
                .is_err()
        );
    }

    #[test]
    fn tls_08_seed_roundtrips_through_secret_data_and_invalid_data_is_no_seed() {
        let generated = Seed::generate(at(5));
        let data = generated.to_data();
        let back = Seed::from_data(Some(&data)).unwrap();
        assert_eq!(back.created, at(5));
        assert_eq!(*back.bytes, *generated.bytes);
        assert_ne!(*generated.bytes, [0; 32]);
        assert_ne!(*Seed::generate(at(5)).bytes, *generated.bytes);

        assert!(Seed::from_data(None).is_none());
        assert!(Seed::from_data(Some(&BTreeMap::new())).is_none());
        let mut short = data.clone();
        short.insert("seed".into(), ByteString(vec![1; 31]));
        assert!(Seed::from_data(Some(&short)).is_none());
        let mut dated = data.clone();
        dated.insert("created".into(), ByteString(b"yesterday".to_vec()));
        assert!(Seed::from_data(Some(&dated)).is_none());
    }

    #[test]
    fn tls_08_the_seed_is_not_in_debug_output() {
        let s = Seed::generate(at(0));
        let shown = format!("{s:?}");
        assert!(shown.contains("created"));
        assert!(!shown.contains("bytes"));
    }
}
