//! Certificat serveur relu à chaud (spec §9.2b) : cert-manager renouvelle le Secret monté
//! bien avant l'expiration, le serveur suit sans redémarrer.

use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use rustls::crypto::CryptoProvider;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls_pki_types::pem::PemObject;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};
use serde::Deserialize;

/// Relecture par défaut des fichiers du certificat.
pub const DEFAULT_RELOAD_INTERVAL: Duration = Duration::from_secs(10);

/// Certificat et clé servis, en PEM (section `tls` des configurations).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TlsFiles {
    pub cert_file: PathBuf,
    pub key_file: PathBuf,
    /// Les fichiers sont relus à cet intervalle (secondes) ; un contenu changé est chargé.
    #[serde(
        default = "default_reload_interval",
        deserialize_with = "duration_secs"
    )]
    pub reload_interval: Duration,
}

impl TlsFiles {
    pub fn new(cert_file: impl Into<PathBuf>, key_file: impl Into<PathBuf>) -> Self {
        Self {
            cert_file: cert_file.into(),
            key_file: key_file.into(),
            reload_interval: DEFAULT_RELOAD_INTERVAL,
        }
    }
}

fn default_reload_interval() -> Duration {
    DEFAULT_RELOAD_INTERVAL
}

fn duration_secs<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
    let secs = f64::deserialize(d)?;
    let interval = Duration::try_from_secs_f64(secs).map_err(serde::de::Error::custom)?;
    if interval.is_zero() {
        return Err(serde::de::Error::custom(
            "reload_interval doit être positif",
        ));
    }
    Ok(interval)
}

/// Issue d'une relecture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reload {
    /// Contenu identique à celui servi.
    Unchanged,
    /// Nouveau contenu chargé : il sert dès la poignée de main suivante.
    Reloaded,
    /// Nouveau contenu illisible : l'ancien certificat reste servi.
    Refused(String),
}

/// Contenu brut des deux fichiers.
type Contents = (Vec<u8>, Vec<u8>);

struct Seen {
    /// Contenu du certificat servi.
    served: Contents,
    /// Dernier refus, pour ne le journaliser qu'une fois.
    refused: Option<Result<Contents, String>>,
}

/// Résout le certificat de chaque poignée de main : le dernier contenu lisible des fichiers.
pub struct CertReloader {
    files: TlsFiles,
    provider: Arc<CryptoProvider>,
    current: RwLock<Arc<CertifiedKey>>,
    /// Date Unix d'expiration du certificat servi (spec §9.2b).
    not_after: AtomicI64,
    seen: Mutex<Seen>,
}

impl std::fmt::Debug for CertReloader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CertReloader")
            .field("files", &self.files)
            .finish_non_exhaustive()
    }
}

fn read(files: &TlsFiles) -> Result<Contents, String> {
    let cert = std::fs::read(&files.cert_file)
        .map_err(|e| format!("certificat {}: {e}", files.cert_file.display()))?;
    let key = std::fs::read(&files.key_file)
        .map_err(|e| format!("clé {}: {e}", files.key_file.display()))?;
    Ok((cert, key))
}

fn parse(
    files: &TlsFiles,
    (cert, key): &Contents,
    provider: &CryptoProvider,
) -> Result<CertifiedKey, String> {
    let chain: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(cert)
        .collect::<Result<_, _>>()
        .map_err(|e| format!("certificat {}: {e}", files.cert_file.display()))?;
    if chain.is_empty() {
        return Err(format!(
            "certificat {}: aucun certificat",
            files.cert_file.display()
        ));
    }
    let key = PrivateKeyDer::from_pem_slice(key)
        .map_err(|e| format!("clé {}: {e}", files.key_file.display()))?;
    // Vérifie aussi que la clé va avec le certificat.
    CertifiedKey::from_der(chain, key, provider).map_err(|e| {
        format!(
            "certificat {} et clé {}: {e}",
            files.cert_file.display(),
            files.key_file.display()
        )
    })
}

/// Date Unix d'expiration du premier certificat de la chaîne ; 0 s'il ne se lit pas.
fn not_after(key: &CertifiedKey) -> i64 {
    key.cert
        .first()
        .and_then(|der| x509_parser::parse_x509_certificate(der).ok())
        .map_or(0, |(_, cert)| cert.validity().not_after.timestamp())
}

impl CertReloader {
    /// Charge le certificat ; illisible, le démarrage échoue.
    pub fn load(files: &TlsFiles) -> Result<Arc<Self>, String> {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let contents = read(files)?;
        let key = parse(files, &contents, &provider)?;
        Ok(Arc::new(Self {
            files: files.clone(),
            provider,
            not_after: AtomicI64::new(not_after(&key)),
            current: RwLock::new(Arc::new(key)),
            seen: Mutex::new(Seen {
                served: contents,
                refused: None,
            }),
        }))
    }

    pub fn files(&self) -> &TlsFiles {
        &self.files
    }

    /// Certificat servi à la prochaine poignée de main.
    pub fn current(&self) -> Arc<CertifiedKey> {
        self.current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Date Unix d'expiration du certificat servi (feuille de la chaîne) ; 0 si illisible.
    pub fn not_after_unix(&self) -> i64 {
        self.not_after.load(Ordering::Relaxed)
    }

    /// Relit les fichiers ; charge un contenu changé et lisible. Un refus est journalisé
    /// une fois par contenu (ou par erreur de lecture), pas à chaque relecture.
    pub fn reload(&self) -> Reload {
        let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
        let read = read(&self.files);
        if let Ok(contents) = &read
            && *contents == seen.served
        {
            seen.refused = None;
            return Reload::Unchanged;
        }
        let parsed = read
            .clone()
            .and_then(|c| parse(&self.files, &c, &self.provider).map(|k| (c, k)));
        match parsed {
            Ok((contents, key)) => {
                self.not_after.store(not_after(&key), Ordering::Relaxed);
                *self.current.write().unwrap_or_else(PoisonError::into_inner) = Arc::new(key);
                seen.served = contents;
                seen.refused = None;
                tracing::info!(cert = %self.files.cert_file.display(), "certificat rechargé");
                Reload::Reloaded
            }
            Err(e) => {
                if seen.refused.as_ref() != Some(&read) {
                    tracing::warn!(error = %e, "nouveau certificat illisible : l'ancien reste servi");
                    seen.refused = Some(read);
                }
                Reload::Refused(e)
            }
        }
    }
}

impl ResolvesServerCert for CertReloader {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.current())
    }
}

/// Configuration TLS d'un serveur dont le certificat est relu à chaud.
#[derive(Clone)]
pub struct Tls {
    pub(crate) reloader: Arc<CertReloader>,
    pub(crate) acceptor: tokio_rustls::TlsAcceptor,
}

impl Tls {
    /// `alpn` : protocoles annoncés (`h2`, `http/1.1`), aucun si vide.
    pub fn new(files: &TlsFiles, alpn: &[&[u8]]) -> Result<Self, String> {
        let reloader = CertReloader::load(files)?;
        let mut server = rustls::ServerConfig::builder_with_provider(reloader.provider.clone())
            .with_safe_default_protocol_versions()
            .map_err(|e| e.to_string())?
            .with_no_client_auth()
            .with_cert_resolver(reloader.clone());
        server.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
        Ok(Self {
            reloader,
            acceptor: tokio_rustls::TlsAcceptor::from(Arc::new(server)),
        })
    }

    pub fn reloader(&self) -> &Arc<CertReloader> {
        &self.reloader
    }
}
