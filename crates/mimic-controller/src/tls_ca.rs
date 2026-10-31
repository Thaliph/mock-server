//! TLS seed and per-deployment root CAs (spec §5, §10.2): the seed is the only secret; a root is
//! a pure function of the seed and `<namespace>/<deployment>`, computed in memory when needed
//! and never stored. Nothing in this module logs or prints key material.

use std::collections::BTreeMap;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use hkdf::Hkdf;
use k8s_openapi::ByteString;
use k8s_openapi::api::core::v1::Secret;
use kube::api::{Api, PostParams};
use mimic_runtime::config::{ConfigError, Env};
use p256::pkcs8::EncodePrivateKey;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Registry;
use rcgen::{
    BasicConstraints, CertificateParams, DnType, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    PKCS_ECDSA_P256_SHA256, PublicKeyData, SerialNumber, SubjectPublicKeyInfo,
};
use sha2::Sha256;
use zeroize::Zeroizing;

use crate::certs::datetime;

const ROOT_LIFETIME: Duration = Duration::from_secs(10 * 365 * 24 * 3600);
const INTERMEDIATE_LIFETIME: Duration = Duration::from_secs(25 * 3600);
/// Certificates are valid from slightly before issuance, against clock skew.
const SKEW: Duration = Duration::from_secs(3600);
/// Separates the use of the seed for roots from any other.
const INFO: &str = "mimic tls root v1 ";

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
    pub fn generate(now: SystemTime) -> Seed {
        let mut bytes = Zeroizing::new([0_u8; 32]);
        rand::fill(&mut bytes[..]);
        Seed {
            bytes,
            created: now,
        }
    }

    /// The seed held by a Secret, if it holds a valid one: 32 bytes under `seed`, a decimal
    /// number of seconds since the Unix epoch under `created`.
    pub fn from_data(data: Option<&BTreeMap<String, ByteString>>) -> Option<Seed> {
        let data = data?;
        let bytes: [u8; 32] = data.get("seed")?.0.as_slice().try_into().ok()?;
        let created = std::str::from_utf8(&data.get("created")?.0)
            .ok()?
            .trim()
            .parse::<u64>()
            .ok()?;
        Some(Seed {
            bytes: Zeroizing::new(bytes),
            created: UNIX_EPOCH + Duration::from_secs(created),
        })
    }

    fn same(&self, other: &Seed) -> bool {
        self.created == other.created && self.bytes[..] == other.bytes[..]
    }

    pub fn to_data(&self) -> BTreeMap<String, ByteString> {
        let created = self
            .created
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        BTreeMap::from([
            ("seed".to_string(), ByteString(self.bytes.to_vec())),
            (
                "created".to_string(),
                ByteString(created.to_string().into_bytes()),
            ),
        ])
    }
}

/// The root CA of one deployment.
pub struct Root {
    issuer: Issuer<'static, KeyPair>,
    // The webhook publishes the certificate in the pod spec; it lands after the sidecar API.
    #[allow(dead_code)]
    certificate: String,
    #[allow(dead_code)]
    public_key: Vec<u8>,
}

impl Root {
    #[allow(dead_code)]
    pub fn certificate_pem(&self) -> &str {
        &self.certificate
    }

    /// SubjectPublicKeyInfo (DER) of the root key.
    #[allow(dead_code)]
    pub fn public_key_der(&self) -> Vec<u8> {
        self.public_key.clone()
    }

    /// Certifies the P-256 key `spki_der` (SubjectPublicKeyInfo, DER) as a path-length-0
    /// intermediate valid from `now - 1 h` for 25 h. Returns the certificate as PEM.
    pub fn sign_intermediate(&self, spki_der: &[u8], now: SystemTime) -> Result<String, String> {
        let subject =
            SubjectPublicKeyInfo::from_der(spki_der).map_err(|e| format!("public key: {e}"))?;
        if PublicKeyData::algorithm(&subject) != &PKCS_ECDSA_P256_SHA256 {
            return Err("public key: expected ECDSA P-256".to_string());
        }
        let mut params = CertificateParams::default();
        params
            .distinguished_name
            .push(DnType::CommonName, "mimic intermediate");
        params
            .distinguished_name
            .push(DnType::OrganizationName, "mimic");
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
        params.not_before = datetime(now - SKEW);
        params.not_after = datetime(now + INTERMEDIATE_LIFETIME);
        let certificate = params
            .signed_by(&subject, &self.issuer)
            .map_err(|e| format!("intermediate certificate: {e}"))?;
        Ok(certificate.pem())
    }
}

/// The root of `<namespace>/<deployment>` under `seed`.
pub fn derive_root(seed: &Seed, namespace: &str, deployment: &str) -> Result<Root, String> {
    let name = format!("{namespace}/{deployment}");
    let hkdf = Hkdf::<Sha256>::new(None, &seed.bytes[..]);
    let expand = |purpose: &str, counter: u8, out: &mut [u8]| {
        let info = format!("{INFO}{purpose} {counter} {name}");
        hkdf.expand(info.as_bytes(), out)
            .map_err(|e| format!("derive {purpose}: {e}"))
    };
    // A 32-byte output is a valid P-256 scalar but for a negligible share of values: next one.
    let secret = (0..=u8::MAX)
        .find_map(|counter| {
            let mut scalar = Zeroizing::new([0_u8; 32]);
            expand("key", counter, &mut scalar[..]).ok()?;
            p256::SecretKey::from_slice(&scalar[..]).ok()
        })
        .ok_or("derive key: no valid scalar")?;
    let der = secret
        .to_pkcs8_der()
        .map_err(|e| format!("root key: {e}"))?;
    let key = KeyPair::try_from(der.as_bytes()).map_err(|e| format!("root key: {e}"))?;

    let mut serial = [0_u8; 16];
    expand("serial", 0, &mut serial)?;
    // A positive integer, never zero.
    serial[0] = (serial[0] & 0x7f) | 0x01;
    let mut params = CertificateParams::default();
    params
        .distinguished_name
        .push(DnType::CommonName, format!("mimic {name}"));
    params
        .distinguished_name
        .push(DnType::OrganizationName, "mimic");
    params.serial_number = Some(SerialNumber::from_slice(&serial));
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign];
    params.not_before = datetime(seed.created - SKEW);
    params.not_after = datetime(seed.created + ROOT_LIFETIME);
    let certificate = params
        .self_signed(&key)
        .map_err(|e| format!("root certificate: {e}"))?
        .pem();
    let public_key = key.subject_public_key_info();
    Ok(Root {
        issuer: Issuer::new(params, key),
        certificate,
        public_key,
    })
}

/// What the controller needs to keep the seed (spec §10.2).
pub struct TlsCaConfig {
    pub namespace: String,
    pub secret: String,
}

impl TlsCaConfig {
    pub fn from_env(env: &Env) -> Result<TlsCaConfig, ConfigError> {
        Ok(TlsCaConfig {
            namespace: env.required("MIMIC_NAMESPACE")?.to_string(),
            secret: env.required("MIMIC_TLS_CA_SECRET")?.to_string(),
        })
    }
}

/// The seed currently in use; empty until the Secret has been read.
#[derive(Default)]
pub struct SeedSlot {
    current: RwLock<Option<Arc<Seed>>>,
}

impl SeedSlot {
    pub fn new() -> SeedSlot {
        SeedSlot::default()
    }

    pub fn is_loaded(&self) -> bool {
        self.get().is_some()
    }

    pub fn get(&self) -> Option<Arc<Seed>> {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Makes `seed` the one in use; whether it differs from the previous one.
    pub(crate) fn set(&self, seed: Seed) -> bool {
        let mut current = self.current.write().unwrap_or_else(PoisonError::into_inner);
        let changed = current.as_ref().is_none_or(|old| !old.same(&seed));
        if changed {
            *current = Some(Arc::new(seed));
        }
        changed
    }
}

pub struct SeedMetrics {
    loaded: Gauge,
}

impl SeedMetrics {
    pub fn register(registry: &mut Registry) -> SeedMetrics {
        let metrics = SeedMetrics {
            loaded: Gauge::default(),
        };
        registry.register(
            "tls_seed_loaded",
            "1 once the TLS seed is loaded",
            metrics.loaded.clone(),
        );
        metrics
    }
}

/// How often the Secret is read again, to follow a seed replaced by someone else.
const RECHECK: Duration = Duration::from_secs(60);
const RETRY: Duration = Duration::from_secs(2);

/// Runs for ever: loads the seed of the Secret into `slot`, generating it when the Secret has
/// none. Replicas converge through the `resourceVersion` of the Secret.
pub async fn manage(
    client: kube::Client,
    config: TlsCaConfig,
    slot: Arc<SeedSlot>,
    metrics: SeedMetrics,
) {
    let secrets = Api::<Secret>::namespaced(client, &config.namespace);
    loop {
        let wait = match reconcile(&secrets, &config.secret, &slot, &metrics).await {
            Ok(()) => RECHECK,
            Err(e) => {
                tracing::warn!(error = %e, "tls seed not in place yet");
                RETRY
            }
        };
        tokio::time::sleep(wait).await;
    }
}

async fn reconcile(
    secrets: &Api<Secret>,
    name: &str,
    slot: &SeedSlot,
    metrics: &SeedMetrics,
) -> Result<(), String> {
    let mut secret = secrets
        .get_opt(name)
        .await
        .map_err(|e| format!("read secret: {e}"))?
        .ok_or_else(|| format!("secret {name} does not exist"))?;
    let seed = match Seed::from_data(secret.data.as_ref()) {
        Some(seed) => seed,
        None => {
            if secret.data.as_ref().is_some_and(|data| !data.is_empty()) {
                tracing::warn!(secret = name, "tls seed invalid, replacing it");
            }
            let seed = Seed::generate(SystemTime::now());
            secret.data = Some(seed.to_data());
            // The resourceVersion read above makes a concurrent writer win: we then start over.
            secrets
                .replace(name, &PostParams::default(), &secret)
                .await
                .map_err(|e| format!("write secret: {e}"))?;
            tracing::info!(secret = name, "tls seed generated");
            seed
        }
    };
    if slot.set(seed) {
        tracing::info!(secret = name, "tls seed loaded");
    }
    metrics.loaded.set(1);
    Ok(())
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
        assert!(
            subject(&a).contains("CN=mimic shop/cart"),
            "{}",
            subject(&a)
        );
        assert!(subject(&a).contains("O=mimic"));
        for (s, ns, dep) in [
            (2, "shop", "cart"),
            (1, "other", "cart"),
            (1, "shop", "pay"),
        ] {
            let b = derive_root(&seed(s), ns, dep).unwrap();
            assert_ne!(a.public_key_der(), b.public_key_der(), "{s} {ns} {dep}");
        }
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

    /// An API server holding one Secret: `GET` answers it, `PUT` replaces it.
    fn secret_server(secret: Arc<std::sync::Mutex<Secret>>) -> kube::Client {
        let service = tower::service_fn(move |request: http::Request<kube::client::Body>| {
            let secret = secret.clone();
            async move {
                assert_eq!(
                    request.uri().path(),
                    "/api/v1/namespaces/mimic-system/secrets/tls-ca"
                );
                if request.method() == http::Method::PUT {
                    let bytes = request.into_body().collect_bytes().await.unwrap();
                    *secret.lock().unwrap() = serde_json::from_slice(&bytes).unwrap();
                }
                let body = serde_json::to_vec(&*secret.lock().unwrap()).unwrap();
                Ok::<_, std::convert::Infallible>(
                    http::Response::builder()
                        .header("content-type", "application/json")
                        .body(kube::client::Body::from(body))
                        .unwrap(),
                )
            }
        });
        kube::Client::new(service, "default")
    }

    fn secret_with(data: Option<BTreeMap<String, ByteString>>) -> Secret {
        let mut secret = Secret::default();
        secret.metadata.name = Some("tls-ca".into());
        secret.metadata.namespace = Some("mimic-system".into());
        secret.data = data;
        secret
    }

    #[tokio::test(start_paused = true)]
    async fn tls_08_a_replaced_seed_signs_intermediates_within_60_seconds() {
        let secret = Arc::new(std::sync::Mutex::new(secret_with(None)));
        let slot = Arc::new(SeedSlot::new());
        let mut registry = Registry::default();
        tokio::spawn(manage(
            secret_server(secret.clone()),
            TlsCaConfig {
                namespace: "mimic-system".into(),
                secret: "tls-ca".into(),
            },
            slot.clone(),
            SeedMetrics::register(&mut registry),
        ));
        tokio::time::sleep(Duration::from_secs(1)).await;
        // An empty Secret is filled and loaded.
        let first = slot.get().expect("seed loaded");
        assert_eq!(
            Seed::from_data(secret.lock().unwrap().data.as_ref())
                .unwrap()
                .bytes[..],
            first.bytes[..]
        );

        // Someone else replaces the seed: within 60 s (+ 5 s) the new root signs.
        let replaced = Seed::generate(at(7));
        secret.lock().unwrap().data = Some(replaced.to_data());
        tokio::time::sleep(Duration::from_secs(65)).await;
        let current = slot.get().unwrap();
        assert_eq!(current.bytes[..], replaced.bytes[..]);

        let key = KeyPair::generate().unwrap();
        let spki = key.subject_public_key_info();
        let signed = derive_root(&current, "shop", "cart")
            .unwrap()
            .sign_intermediate(&spki, at(100))
            .unwrap();
        let expected = derive_root(&replaced, "shop", "cart").unwrap();
        let inter_der = parse(&signed);
        let (_, inter) = x509_parser::parse_x509_certificate(&inter_der).unwrap();
        let root_der = parse(expected.certificate_pem());
        let (_, root) = x509_parser::parse_x509_certificate(&root_der).unwrap();
        inter.verify_signature(Some(root.public_key())).unwrap();
    }
}
