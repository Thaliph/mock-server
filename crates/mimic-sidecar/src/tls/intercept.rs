//! The interception path (spec §5): the decision for an outbound ClientHello, the sidecar's own
//! handshake with the destination (done first, against the upstream roots) and the handshake
//! that answers the client with a leaf for its SNI.

use std::fs;
use std::io;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mimic_core::proxy::tls::{
    Bounded, ClientHello, Decision, DestKey, Exclusions, MEMORY_CAPACITY, Reason, dest_key,
};
use mimic_runtime::metrics::Metrics;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use rustls::SignatureScheme;
use rustls::client::ResolvesClientCert;
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName};
use rustls::server::{ClientHello as ServerHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls::{ClientConfig, RootCertStore, ServerConfig};
use tokio::net::TcpStream;
use tokio::sync::OnceCell;
use tokio_rustls::{TlsAcceptor, TlsConnector, client};
use x509_parser::pem::Pem;

use super::ca::CaState;
use crate::transport::Socket;

/// Both handshakes are bounded: a peer that stalls must not hold the connection forever.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// Every value of the `interception` label (spec §9); the series exist from the start.
const DECISIONS: [&str; 7] = [
    "intercepted",
    "excluded",
    "no_ca",
    "unsupported",
    "upstream_untrusted",
    "client_cert_requested",
    "client_rejected",
];

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct InterceptionLabel {
    interception: &'static str,
}

/// `mimic_tls_connections_total{interception}` and `mimic_tls_passthrough_destinations`.
#[derive(Clone, Default)]
pub struct TlsMetrics {
    connections: Family<InterceptionLabel, Counter>,
    remembered: Gauge,
}

impl TlsMetrics {
    pub fn register(metrics: &mut Metrics) -> TlsMetrics {
        let tls = TlsMetrics::default();
        for interception in DECISIONS {
            let _ = tls
                .connections
                .get_or_create(&InterceptionLabel { interception });
        }
        let registry = metrics.registry_mut();
        registry.register(
            "tls_connections",
            "Outbound TLS connections, by interception decision",
            tls.connections.clone(),
        );
        registry.register(
            "tls_passthrough_destinations",
            "Destinations remembered as passed through",
            tls.remembered.clone(),
        );
        tls
    }
}

/// A leaf that is already chosen, whatever the ClientHello says.
#[derive(Debug)]
struct Fixed(Arc<CertifiedKey>);

impl ResolvesServerCert for Fixed {
    fn resolve(&self, _: ServerHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.0.clone())
    }
}

/// Everything the connections need to decide about and perform an interception.
pub struct Interception {
    exclusions: Exclusions,
    ca: Option<Arc<CaState>>,
    remembered: Mutex<Bounded<DestKey, Reason>>,
    /// Loaded at the first interception (spec §5).
    roots: OnceCell<Arc<RootCertStore>>,
    system_stores: Vec<PathBuf>,
    trust_dir: PathBuf,
    metrics: TlsMetrics,
}

impl Default for Interception {
    /// No CA: every outbound TLS connection is passed through (`no_ca`).
    fn default() -> Self {
        Interception::new(
            Exclusions::default(),
            None,
            Vec::new(),
            PathBuf::new(),
            TlsMetrics::default(),
        )
    }
}

impl Interception {
    /// `system_stores`: candidates for the sidecar's own store (the first existing is used);
    /// `trust_dir`: the `mimic-trust` volume, whose `trust/*/image-ca.pem` are added.
    pub fn new(
        exclusions: Exclusions,
        ca: Option<Arc<CaState>>,
        system_stores: Vec<PathBuf>,
        trust_dir: PathBuf,
        metrics: TlsMetrics,
    ) -> Interception {
        Interception {
            exclusions,
            ca,
            remembered: Mutex::new(Bounded::new(MEMORY_CAPACITY)),
            roots: OnceCell::new(),
            system_stores,
            trust_dir,
            metrics,
        }
    }

    /// Counts an outbound TLS connection under its decision.
    pub(crate) fn count(&self, interception: &'static str) {
        self.metrics
            .connections
            .get_or_create(&InterceptionLabel { interception })
            .inc();
    }

    /// The §5 decision for `hello` addressed to `dst`, and, when intercepting, the leaf to
    /// answer the client with.
    pub(crate) fn decide(&self, hello: &ClientHello, dst: SocketAddr) -> Result<Plan, Reason> {
        let has_ca = self.ca.as_ref().is_some_and(|ca| ca.usable());
        let decision = {
            let remembered = self
                .remembered
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            mimic_core::proxy::tls::decide(
                hello,
                dst.ip(),
                dst.port(),
                &self.exclusions,
                &remembered,
                has_ca,
            )
        };
        let alpn = match decision {
            Decision::PassThrough(reason) => return Err(reason),
            Decision::Intercept { alpn } => alpn,
        };
        let name = match &hello.sni {
            Some(sni) => sni.clone(),
            None => dst.ip().to_string(),
        };
        let server_name = match hello.sni.as_deref() {
            Some(sni) => ServerName::try_from(sni.trim_end_matches('.').to_string()),
            None => Ok(ServerName::IpAddress(dst.ip().into())),
        };
        // A name that cannot be verified or certified is not one mimic can reproduce.
        let (Ok(server_name), Some(leaf)) =
            (server_name, self.ca.as_ref().and_then(|ca| ca.leaf(&name)))
        else {
            return Err(Reason::Unsupported);
        };
        Ok(Plan {
            alpn,
            server_name,
            leaf,
            key: dest_key(hello, dst.ip(), dst.port()),
            sni: hello.sni.clone(),
        })
    }

    /// The upstream roots, loaded once. A load that fails is not remembered: the next
    /// interception tries again.
    async fn roots(&self) -> io::Result<Arc<RootCertStore>> {
        self.roots
            .get_or_try_init(|| async {
                let (stores, dir) = (self.system_stores.clone(), self.trust_dir.clone());
                let (roots, skipped) =
                    tokio::task::spawn_blocking(move || load_roots(&stores, &dir))
                        .await
                        .map_err(|e| io::Error::other(format!("loading the roots: {e}")))?;
                for skip in skipped {
                    tracing::warn!(
                        target: "tls",
                        path = %skip.path.display(),
                        skipped = skip.blocks,
                        error = skip.error,
                        "tls roots skipped"
                    );
                }
                Ok(Arc::new(roots))
            })
            .await
            .cloned()
    }

    /// The sidecar's own handshake with the destination on `upstream`, offering the client's
    /// SNI and `plan.alpn`, verified against the upstream roots.
    pub(crate) async fn connect(
        &self,
        upstream: TcpStream,
        plan: &Plan,
    ) -> Result<client::TlsStream<Socket>, ConnectError> {
        let roots = self.roots().await.map_err(ConnectError::Io)?;
        let requested = Arc::new(CertRequested::default());
        let mut config = ClientConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(|e| ConnectError::Io(io::Error::other(e)))?
            .with_root_certificates(roots)
            .with_client_cert_resolver(requested.clone());
        config.alpn_protocols = plan.alpn.iter().map(|p| p.as_bytes().to_vec()).collect();
        let connector = TlsConnector::from(Arc::new(config));
        let handshake = connector.connect(plan.server_name.clone(), Socket::new(upstream));
        let result = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "handshake timeout"));
        if requested.0.load(Ordering::Relaxed) {
            return Err(ConnectError::Reason(Reason::ClientCertRequested));
        }
        match result {
            Ok(Ok(stream)) => Ok(stream),
            // A TLS-level failure (certificate, alert, version, cipher) is the destination's
            // verdict; anything else is the connection failing under the handshake.
            Ok(Err(e)) if e.get_ref().is_some_and(|inner| inner.is::<rustls::Error>()) => {
                Err(ConnectError::Reason(Reason::UpstreamUntrusted))
            }
            Ok(Err(e)) | Err(e) => Err(ConnectError::Io(e)),
        }
    }

    /// Counts the connection under `reason` and remembers the destination of `plan`; the first
    /// time it is remembered, logs the WARN `tls passthrough`.
    pub(crate) fn passthrough(&self, plan: &Plan, reason: Reason, dst: SocketAddr) {
        self.count(reason.as_str());
        let mut remembered = self
            .remembered
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if remembered.insert(plan.key.clone(), reason) {
            self.metrics
                .remembered
                .set(i64::try_from(remembered.len()).unwrap_or(i64::MAX));
            tracing::warn!(
                target: "tls",
                sni = plan.sni.as_deref(),
                %dst,
                reason = reason.as_str(),
                "tls passthrough"
            );
        }
    }

    /// The client's handshake on `client`, whose first `hello` bytes were already read,
    /// answered with `plan.leaf` and the ALPN protocol the destination chose.
    pub(crate) async fn accept(
        &self,
        client: TcpStream,
        hello: Vec<u8>,
        plan: &Plan,
        alpn: Option<Vec<u8>>,
    ) -> Result<tokio_rustls::server::TlsStream<Socket>, AcceptError> {
        let mut config = ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()
            .map_err(|e| AcceptError::Failed(io::Error::other(e)))?
            .with_no_client_auth()
            .with_cert_resolver(Arc::new(Fixed(plan.leaf.clone())));
        config.alpn_protocols = alpn.into_iter().collect();
        let acceptor = TlsAcceptor::from(Arc::new(config));
        let handshake = acceptor.accept(Socket::with_prefix(client, hello));
        let result = tokio::time::timeout(HANDSHAKE_TIMEOUT, handshake)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "handshake timeout"))
            .and_then(|handshake| handshake);
        result.map_err(|e| {
            if client_gave_up(&e) {
                AcceptError::Rejected
            } else {
                AcceptError::Failed(e)
            }
        })
    }
}

/// Why the handshake with the client did not complete.
pub(crate) enum AcceptError {
    /// The client answered the mimic certificate with an alert, or closed right after it: it
    /// does not trust it (spec §5).
    Rejected,
    /// A reset, a timeout or a malformed message: nothing is learned about the client's trust.
    Failed(io::Error),
}

/// An alert from the client, or the end of its stream in the middle of the handshake (the
/// sidecar's flight with the certificate was already written by then).
fn client_gave_up(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::UnexpectedEof
        || e.get_ref()
            .and_then(|inner| inner.downcast_ref::<rustls::Error>())
            .is_some_and(|inner| matches!(inner, rustls::Error::AlertReceived(_)))
}

/// Why the sidecar's own handshake with the destination did not complete.
pub(crate) enum ConnectError {
    /// Pass the connection through for this reason.
    Reason(Reason),
    /// The connection failed (reset, close, timeout): the client sees the same.
    Io(io::Error),
}

/// Set when the destination sends a `CertificateRequest`; the sidecar has no certificate to
/// answer with, so the connection is passed through for the client to answer itself.
#[derive(Debug, Default)]
struct CertRequested(AtomicBool);

impl ResolvesClientCert for CertRequested {
    fn resolve(&self, _: &[&[u8]], _: &[SignatureScheme]) -> Option<Arc<CertifiedKey>> {
        self.0.store(true, Ordering::Relaxed);
        None
    }

    fn has_certs(&self) -> bool {
        false
    }
}

/// What an interception needs once decided.
pub(crate) struct Plan {
    key: DestKey,
    sni: Option<String>,
    /// ALPN offered to the destination (client order, only `h2` and `http/1.1`).
    alpn: Vec<String>,
    server_name: ServerName<'static>,
    leaf: Arc<CertifiedKey>,
}

fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// The first existing of `stores`, plus `dir/trust/*/image-ca.pem`. Unreadable files are
/// skipped: a destination they would have verified is then passed through.
fn load_roots(stores: &[PathBuf], dir: &Path) -> (RootCertStore, Vec<Skipped>) {
    let mut roots = RootCertStore::empty();
    let mut skipped = Vec::new();
    if let Some(store) = stores.iter().find(|s| s.is_file()) {
        skipped.extend(add_pem(&mut roots, store));
    }
    let mut images: Vec<PathBuf> = fs::read_dir(dir.join("trust"))
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path().join("image-ca.pem"))
        .collect();
    images.sort();
    for image in images {
        // An image without a bundle (no `mimic-trust` run for it) is not an error.
        if image.exists() || image.is_symlink() {
            skipped.extend(add_pem(&mut roots, &image));
        }
    }
    (roots, skipped)
}

/// What a root file did not give.
struct Skipped {
    path: PathBuf,
    /// PEM blocks that did not parse as certificates.
    blocks: usize,
    /// The file could not be read.
    error: Option<String>,
}

fn add_pem(roots: &mut RootCertStore, path: &Path) -> Option<Skipped> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => {
            return Some(Skipped {
                path: path.to_path_buf(),
                blocks: 0,
                error: Some(e.kind().to_string()),
            });
        }
    };
    let mut malformed = 0;
    let certs: Vec<_> = Pem::iter_from_buffer(&bytes)
        .map_while(|pem| {
            if pem.is_err() {
                malformed += 1;
            }
            pem.ok()
        })
        .filter(|p| p.label == "CERTIFICATE")
        .map(|p| CertificateDer::from(p.contents))
        .collect();
    let (_, ignored) = roots.add_parsable_certificates(certs);
    let blocks = malformed + ignored;
    (blocks > 0).then(|| Skipped {
        path: path.to_path_buf(),
        blocks,
        error: None,
    })
}
