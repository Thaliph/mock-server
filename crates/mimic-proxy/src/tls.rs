//! Interception TLS sortante (spec §4.2) : autorité de certification, certificats forgés à
//! la volée, lecture du SNI sans consommer la connexion, client TLS vers l'amont.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};

use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::ServerConfig;
use rustls::client::ClientConfig;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls_pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

use crate::config::TlsConfig;

/// Nom utilisé quand le client n'envoie pas de SNI.
const FALLBACK_NAME: &str = "localhost";
/// Au-delà, le cache des certificats forgés est vidé.
const MAX_CACHED: usize = 1_000;

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Autorité de certification du sidecar.
pub struct Ca {
    issuer: Issuer<'static, KeyPair>,
    cert_der: CertificateDer<'static>,
    cert_pem: String,
    cache: Mutex<HashMap<String, Arc<CertifiedKey>>>,
}

impl std::fmt::Debug for Ca {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Ca")
    }
}

impl Ca {
    /// CA éphémère, jamais écrite sur disque côté clé.
    pub fn generate() -> Result<Self, String> {
        let mut params = CertificateParams::new(Vec::<String>::new()).map_err(err)?;
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "mimic sidecar CA");
        dn.push(DnType::OrganizationName, "mimic");
        params.distinguished_name = dn;
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
            KeyUsagePurpose::DigitalSignature,
        ];
        let key = KeyPair::generate().map_err(err)?;
        let cert = params.self_signed(&key).map_err(err)?;
        Ok(Self {
            cert_der: cert.der().clone(),
            cert_pem: cert.pem(),
            issuer: Issuer::new(params, key),
            cache: Mutex::new(HashMap::new()),
        })
    }

    /// CA fournie (Secret, cert-manager).
    pub fn from_pem(cert_pem: &str, key_pem: &str) -> Result<Self, String> {
        let key = KeyPair::from_pem(key_pem).map_err(err)?;
        let issuer = Issuer::from_ca_cert_pem(cert_pem, key).map_err(err)?;
        let cert_der = pem_certs(cert_pem)?
            .into_iter()
            .next()
            .ok_or("certificat de CA vide")?;
        Ok(Self {
            issuer,
            cert_der,
            cert_pem: cert_pem.to_string(),
            cache: Mutex::new(HashMap::new()),
        })
    }

    pub fn load(config: &TlsConfig) -> Result<Self, String> {
        match (&config.ca_cert_file, &config.ca_key_file) {
            (Some(c), Some(k)) => {
                let cert =
                    std::fs::read_to_string(c).map_err(|e| format!("{}: {e}", c.display()))?;
                let key =
                    std::fs::read_to_string(k).map_err(|e| format!("{}: {e}", k.display()))?;
                Self::from_pem(&cert, &key)
            }
            (None, None) => Self::generate(),
            _ => Err("tls.ca_cert_file et tls.ca_key_file vont ensemble".into()),
        }
    }

    pub fn cert_pem(&self) -> &str {
        &self.cert_pem
    }

    /// Écrit `ca.crt` et `ca-bundle.crt` (racines système + CA) pour l'application.
    pub fn publish(&self, dir: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        std::fs::write(dir.join("ca.crt"), &self.cert_pem).map_err(|e| e.to_string())?;
        let mut bundle = String::new();
        for system in [
            "/etc/ssl/certs/ca-certificates.crt",
            "/etc/pki/tls/certs/ca-bundle.crt",
            "/etc/ssl/cert.pem",
        ] {
            if let Ok(s) = std::fs::read_to_string(system) {
                bundle.push_str(&s);
                if !bundle.ends_with('\n') {
                    bundle.push('\n');
                }
                break;
            }
        }
        bundle.push_str(&self.cert_pem);
        std::fs::write(dir.join("ca-bundle.crt"), bundle).map_err(|e| e.to_string())
    }

    /// Certificat feuille pour `host`, signé par la CA, mis en cache.
    pub fn leaf(&self, host: &str) -> Result<Arc<CertifiedKey>, String> {
        let host = host.to_ascii_lowercase();
        if let Some(c) = self
            .cache
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&host)
        {
            return Ok(c.clone());
        }
        let mut params = CertificateParams::new(vec![host.clone()]).map_err(err)?;
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, host.as_str());
        params.distinguished_name = dn;
        params.use_authority_key_identifier_extension = true;
        params.key_usages = vec![
            KeyUsagePurpose::DigitalSignature,
            KeyUsagePurpose::KeyEncipherment,
        ];
        params.extended_key_usages = vec![rcgen::ExtendedKeyUsagePurpose::ServerAuth];
        let key = KeyPair::generate().map_err(err)?;
        let cert = params.signed_by(&key, &self.issuer).map_err(err)?;
        let der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key.serialize_der()));
        let signing = rustls::crypto::ring::sign::any_supported_type(&der).map_err(err)?;
        let certified = Arc::new(CertifiedKey::new(
            vec![cert.der().clone(), self.cert_der.clone()],
            signing,
        ));
        let mut cache = self.cache.lock().unwrap_or_else(PoisonError::into_inner);
        if cache.len() >= MAX_CACHED {
            cache.clear();
        }
        cache.insert(host, certified.clone());
        Ok(certified)
    }

    /// Configuration serveur qui forge un certificat pour le SNI demandé.
    pub fn server_config(self: &Arc<Self>) -> Result<Arc<ServerConfig>, String> {
        let mut config = ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(err)?
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(Resolver(self.clone())));
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        Ok(Arc::new(config))
    }
}

#[derive(Debug)]
struct Resolver(Arc<Ca>);

impl ResolvesServerCert for Resolver {
    fn resolve(&self, hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        let name = hello.server_name().unwrap_or(FALLBACK_NAME);
        match self.0.leaf(name) {
            Ok(c) => Some(c),
            Err(e) => {
                tracing::warn!(%name, error = %e, "certificat forgé impossible");
                None
            }
        }
    }
}

/// Client TLS vers l'amont : racines système, plus une CA additionnelle optionnelle.
pub fn client_config(extra_ca: Option<&Path>) -> Result<ClientConfig, String> {
    let mut roots = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().certs {
        let _ = roots.add(cert);
    }
    if let Some(path) = extra_ca {
        let pem = std::fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
        for cert in pem_certs(&pem)? {
            roots.add(cert).map_err(err)?;
        }
    }
    // ALPN est fixé par le connecteur hyper-rustls.
    Ok(ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(err)?
        .with_root_certificates(roots)
        .with_no_client_auth())
}

/// Configuration client qui ne fait confiance qu'aux certificats de `ca` (CA du contrôleur).
pub fn pinned_client_config(ca: &Path) -> Result<ClientConfig, String> {
    let pem = std::fs::read_to_string(ca).map_err(|e| format!("{}: {e}", ca.display()))?;
    let mut roots = rustls::RootCertStore::empty();
    for cert in pem_certs(&pem)? {
        roots.add(cert).map_err(err)?;
    }
    if roots.is_empty() {
        return Err(format!("{}: aucun certificat", ca.display()));
    }
    Ok(ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .map_err(err)?
        .with_root_certificates(roots)
        .with_no_client_auth())
}

fn pem_certs(pem: &str) -> Result<Vec<CertificateDer<'static>>, String> {
    use rustls_pki_types::pem::PemObject;
    CertificateDer::pem_slice_iter(pem.as_bytes())
        .collect::<Result<Vec<_>, _>>()
        .map_err(err)
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

/// Vrai si `host` correspond à un motif de `tls.intercept` (`api.x.com`, `*.x.com`, `*`).
pub fn intercepted(patterns: &[String], host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    patterns.iter().any(|p| {
        let p = p.to_ascii_lowercase();
        if p == "*" {
            true
        } else if let Some(suffix) = p.strip_prefix("*.") {
            host.len() > suffix.len() + 1
                && host.ends_with(suffix)
                && host.as_bytes()[host.len() - suffix.len() - 1] == b'.'
        } else {
            host == p
        }
    })
}

/// Nom demandé (SNI) dans un ClientHello TLS, à partir des premiers octets de la connexion.
/// `None` si les octets ne suffisent pas ou ne contiennent pas de SNI.
pub fn parse_sni(data: &[u8]) -> Option<String> {
    let mut r = Reader(data);
    if r.u8()? != 0x16 {
        return None;
    }
    r.skip(2)?; // version de l'enregistrement
    let record_len = r.u16()? as usize;
    let mut hs = Reader(r.take(record_len)?);
    if hs.u8()? != 0x01 {
        return None; // pas un ClientHello
    }
    let len = hs.u24()? as usize;
    let mut hello = Reader(hs.take(len)?);
    hello.skip(2 + 32)?; // version, aléa
    let sid = hello.u8()? as usize;
    hello.skip(sid)?;
    let suites = hello.u16()? as usize;
    hello.skip(suites)?;
    let compression = hello.u8()? as usize;
    hello.skip(compression)?;
    let ext_len = hello.u16()? as usize;
    let mut exts = Reader(hello.take(ext_len)?);
    while let (Some(kind), Some(len)) = (exts.u16(), exts.u16()) {
        let body = exts.take(len as usize)?;
        if kind != 0 {
            continue;
        }
        let mut sni = Reader(body);
        let list = sni.u16()? as usize;
        let mut list = Reader(sni.take(list)?);
        while let Some(name_type) = list.u8() {
            let n = list.u16()? as usize;
            let name = list.take(n)?;
            if name_type == 0 {
                return std::str::from_utf8(name).ok().map(str::to_ascii_lowercase);
            }
        }
        return None;
    }
    None
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (head, tail) = self.0.split_at(n);
        self.0 = tail;
        Some(head)
    }
    fn skip(&mut self, n: usize) -> Option<()> {
        self.take(n).map(|_| ())
    }
    fn u8(&mut self) -> Option<u8> {
        self.take(1).map(|b| b[0])
    }
    fn u16(&mut self) -> Option<u16> {
        self.take(2).map(|b| u16::from_be_bytes([b[0], b[1]]))
    }
    fn u24(&mut self) -> Option<u32> {
        self.take(3)
            .map(|b| u32::from_be_bytes([0, b[0], b[1], b[2]]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_hello(name: &str) -> Vec<u8> {
        let config = client_config(None).unwrap();
        let server_name = rustls_pki_types::ServerName::try_from(name.to_string()).unwrap();
        let mut conn = rustls::ClientConnection::new(Arc::new(config), server_name).unwrap();
        let mut out = vec![];
        conn.write_tls(&mut out).unwrap();
        out
    }

    #[test]
    fn sni_is_read_from_a_real_client_hello() {
        let hello = client_hello("API.Stripe.com");
        assert_eq!(parse_sni(&hello).as_deref(), Some("api.stripe.com"));
        // Préfixe incomplet : pas de conclusion hâtive.
        assert_eq!(parse_sni(&hello[..20]), None);
        assert_eq!(parse_sni(b"GET / HTTP/1.1"), None);
    }

    #[test]
    fn intercept_patterns() {
        let p = vec!["api.stripe.com".to_string(), "*.salesforce.com".to_string()];
        assert!(intercepted(&p, "api.stripe.com"));
        assert!(intercepted(&p, "eu1.salesforce.com"));
        assert!(!intercepted(&p, "salesforce.com"));
        assert!(!intercepted(&p, "evilsalesforce.com"));
        assert!(!intercepted(&p, "stripe.com"));
        assert!(intercepted(&["*".to_string()], "anything.io"));
    }

    #[test]
    fn forged_leaf_chains_to_the_ca() {
        let ca = Arc::new(Ca::generate().unwrap());
        let leaf = ca.leaf("api.stripe.com").unwrap();
        assert_eq!(leaf.cert.len(), 2);
        assert!(Arc::ptr_eq(&leaf, &ca.leaf("API.stripe.com").unwrap()));
        assert!(ca.cert_pem().starts_with("-----BEGIN CERTIFICATE-----"));

        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        let key = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        let provided = Ca::from_pem(&cert.pem(), &key.serialize_pem()).unwrap();
        assert!(provided.leaf("x.io").is_ok());
    }
}
