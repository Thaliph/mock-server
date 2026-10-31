//! The sidecar's CA client (spec §5, §7.2): an intermediate key that lives in memory only, its
//! certification by the controller, renewal at half-life, and leaf certificates issued per SNI.

use std::net::IpAddr;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use mimic_core::proxy::tls::{Backoff, Bounded, MEMORY_CAPACITY, Validity, ca_usable, fetch_due};
use mimic_runtime::metrics::Metrics;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Unit;
use rcgen::{
    CertificateParams, DnType, ExtendedKeyUsagePurpose, IsCa, Issuer, KeyPair, KeyUsagePurpose,
    PublicKeyData, SanType,
};
use rustls::crypto::ring::sign::any_ecdsa_type;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::sign::{CertifiedKey, SigningKey};
use time::OffsetDateTime;
use x509_parser::certificate::X509Certificate;
use x509_parser::pem::Pem;
use x509_parser::prelude::FromDer;

use super::CaConfig;

const ENDPOINT: &str = "/v1/tls/intermediate";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// The controller's answer is one certificate; anything larger is not from it.
const MAX_RESPONSE: usize = 64 * 1024;

/// DER of the first certificate in `pem`, if it parses.
pub fn first_certificate(pem: &str) -> Option<Vec<u8>> {
    Pem::iter_from_buffer(pem.as_bytes())
        .map_while(Result::ok)
        .find(|p| p.label == "CERTIFICATE")
        .map(|p| p.contents)
        .filter(|der| X509Certificate::from_der(der).is_ok())
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct ResultLabel {
    result: &'static str,
}

/// `mimic_tls_intermediate_requests_total{result}` and
/// `mimic_tls_intermediate_expiry_timestamp_seconds` (spec §9).
#[derive(Clone, Default)]
pub struct CaMetrics {
    requests: Family<ResultLabel, Counter>,
    expiry: Gauge,
}

impl CaMetrics {
    pub fn register(metrics: &mut Metrics) -> CaMetrics {
        let ca = CaMetrics::default();
        // Every series exists from the start, at 0.
        for result in ["ok", "error", "mismatch"] {
            let _ = ca.requests.get_or_create(&ResultLabel { result });
        }
        let registry = metrics.registry_mut();
        registry.register(
            "tls_intermediate_requests",
            "Intermediate certification requests to the controller",
            ca.requests.clone(),
        );
        registry.register_with_unit(
            "tls_intermediate_expiry_timestamp",
            "notAfter of the current intermediate, 0 when none",
            Unit::Seconds,
            ca.expiry.clone(),
        );
        ca
    }

    fn count(&self, result: &'static str) {
        self.requests.get_or_create(&ResultLabel { result }).inc();
    }
}

/// An accepted intermediate: its window, its issuing handle and its certificate.
struct Current {
    validity: Validity,
    issuer: Issuer<'static, KeyPair>,
    der: CertificateDer<'static>,
}

struct Inner {
    current: Option<Current>,
    /// Leaves of the current intermediate by name; emptied when it changes.
    leaves: Bounded<String, Arc<CertifiedKey>>,
}

/// The intermediate and the leaves issued under it. Shared by the renewal task and the
/// connections.
pub struct CaState {
    inner: Mutex<Inner>,
    /// One leaf key for the whole process (spec §5).
    leaf_key: KeyPair,
    leaf_signer: Arc<dyn SigningKey>,
    metrics: CaMetrics,
}

impl CaState {
    pub fn new(metrics: CaMetrics) -> Result<CaState, String> {
        let leaf_key = KeyPair::generate().map_err(|e| e.to_string())?;
        let der = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(leaf_key.serialize_der()));
        let leaf_signer = any_ecdsa_type(&der).map_err(|e| e.to_string())?;
        Ok(CaState {
            inner: Mutex::new(Inner {
                current: None,
                leaves: Bounded::new(MEMORY_CAPACITY),
            }),
            leaf_key,
            leaf_signer,
            metrics,
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A panic while holding the lock leaves plain data behind; carry on with it.
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Window of the current intermediate, if any.
    pub fn validity(&self) -> Option<Validity> {
        self.lock().current.as_ref().map(|c| c.validity)
    }

    /// An intermediate exists and is within its validity now.
    pub fn usable(&self) -> bool {
        ca_usable(self.validity(), unix_now())
    }

    /// Installs `pem` as the intermediate for `key` if it chains to `root_der`, as the renewal
    /// task does with the controller's answer (for embedders and tests without a controller).
    pub fn install_intermediate(
        &self,
        root_der: &[u8],
        key: KeyPair,
        pem: &str,
    ) -> Result<(), String> {
        match accept(root_der, key, pem, unix_now()) {
            Ok(current) => {
                self.install(current);
                Ok(())
            }
            Err(Failure::Error(e) | Failure::Mismatch(e)) => Err(e),
        }
    }

    fn install(&self, current: Current) {
        self.metrics
            .expiry
            .set(i64::try_from(current.validity.not_after).unwrap_or(i64::MAX));
        let mut inner = self.lock();
        inner.current = Some(current);
        inner.leaves.clear();
    }

    /// Drops an intermediate that is no longer within its validity.
    fn expire(&self) {
        let mut inner = self.lock();
        if inner
            .current
            .as_ref()
            .is_some_and(|c| !c.validity.usable(unix_now()))
        {
            inner.current = None;
            inner.leaves.clear();
            self.metrics.expiry.set(0);
        }
    }

    /// The certified leaf for `name` (a DNS name, or an IP literal for a ClientHello without
    /// SNI), issued on first use. `None` without a usable intermediate.
    pub fn leaf(&self, name: &str) -> Option<Arc<CertifiedKey>> {
        let name = name.trim_end_matches('.').to_ascii_lowercase();
        let mut inner = self.lock();
        let current = inner.current.as_ref()?;
        if !current.validity.usable(unix_now()) {
            return None;
        }
        if let Some(leaf) = inner.leaves.get(&name) {
            return Some(leaf.clone());
        }
        let certified = self.issue(current, &name).ok()?;
        inner.leaves.insert(name, certified.clone());
        Some(certified)
    }

    fn issue(&self, ca: &Current, name: &str) -> Result<Arc<CertifiedKey>, rcgen::Error> {
        let mut params = CertificateParams::default();
        params.subject_alt_names = vec![match name.parse::<IpAddr>() {
            Ok(ip) => SanType::IpAddress(ip),
            Err(_) => SanType::DnsName(name.try_into()?),
        }];
        params
            .distinguished_name
            .push(DnType::CommonName, name.to_string());
        params.is_ca = IsCa::NoCa;
        params.key_usages = vec![KeyUsagePurpose::DigitalSignature];
        params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let window = ca.validity.leaf();
        params.not_before = at(window.not_before);
        params.not_after = at(window.not_after);
        let leaf = params.signed_by(&self.leaf_key, &ca.issuer)?;
        Ok(Arc::new(CertifiedKey::new(
            vec![leaf.der().clone(), ca.der.clone()],
            self.leaf_signer.clone(),
        )))
    }
}

fn at(secs: u64) -> OffsetDateTime {
    OffsetDateTime::from_unix_timestamp(i64::try_from(secs).unwrap_or(i64::MAX))
        .unwrap_or(OffsetDateTime::UNIX_EPOCH)
}

/// Why a request produced no usable intermediate.
enum Failure {
    /// Transport, status or answer problem: retry.
    Error(String),
    /// The controller's certificate does not chain to `MIMIC_TLS_CA`.
    Mismatch(String),
}

/// Requests, renews and installs the intermediate.
pub struct CaClient {
    config: CaConfig,
    state: Arc<CaState>,
    http: reqwest::Client,
}

impl CaClient {
    pub fn new(config: CaConfig, state: Arc<CaState>) -> Result<CaClient, String> {
        // Already installed by an earlier call: the existing provider is the same one.
        let _ = rustls::crypto::ring::default_provider().install_default();
        let anchor = reqwest::Certificate::from_pem(config.controller_ca.as_bytes())
            .map_err(|e| e.to_string())?;
        let http = reqwest::Client::builder()
            .tls_certs_only([anchor])
            .timeout(REQUEST_TIMEOUT)
            .redirect(reqwest::redirect::Policy::none())
            .no_proxy()
            .build()
            .map_err(|e| e.to_string())?;
        Ok(CaClient {
            config,
            state,
            http,
        })
    }

    /// Keeps an intermediate current for the life of the process.
    pub async fn run(self) {
        let mut backoff = Backoff::default();
        loop {
            let now = unix_now();
            self.state.expire();
            let validity = self.state.validity();
            if !fetch_due(validity, now) {
                tokio::time::sleep(until_renewal(validity, now)).await;
                continue;
            }
            match self.fetch().await {
                Ok(current) => {
                    backoff.success();
                    self.state.metrics.count("ok");
                    tracing::info!(
                        not_after = current.validity.not_after,
                        "intermediate certificate installed"
                    );
                    self.state.install(current);
                }
                Err(failure) => {
                    let wait = backoff.failure();
                    match failure {
                        Failure::Error(reason) => {
                            self.state.metrics.count("error");
                            tracing::warn!(%reason, retry_in_seconds = wait.as_secs(), "intermediate request failed");
                        }
                        Failure::Mismatch(reason) => {
                            self.state.metrics.count("mismatch");
                            tracing::error!(%reason, retry_in_seconds = wait.as_secs(), "tls ca mismatch");
                        }
                    }
                    tokio::time::sleep(wait).await;
                }
            }
        }
    }

    async fn fetch(&self) -> Result<Current, Failure> {
        let key = KeyPair::generate().map_err(|e| Failure::Error(e.to_string()))?;
        // Read for every request: the kubelet rotates the projected token.
        let token = tokio::fs::read_to_string(&self.config.token_file)
            .await
            .map_err(|e| Failure::Error(format!("token file: {}", e.kind())))?;
        let body = serde_json::json!({
            "deployment": self.config.deployment,
            "public_key": STANDARD.encode(key.subject_public_key_info()),
        });
        let response = self
            .http
            .post(format!("{}{ENDPOINT}", self.config.controller_url))
            .bearer_auth(token.trim())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body.to_string())
            .send()
            .await
            // Without the URL: it is the configured one, and nothing else is useful here.
            .map_err(|e| Failure::Error(e.without_url().to_string()))?;
        let status = response.status();
        if !status.is_success() {
            return Err(Failure::Error(format!("status {status}")));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| Failure::Error(e.without_url().to_string()))?;
        if bytes.len() > MAX_RESPONSE {
            return Err(Failure::Error("answer too large".into()));
        }
        let answer: Answer = serde_json::from_slice(&bytes)
            .map_err(|_| Failure::Error("malformed answer".into()))?;
        accept(&self.config.root_der, key, &answer.certificate, unix_now())
    }
}

#[derive(serde::Deserialize)]
struct Answer {
    certificate: String,
}

/// Sleep until the renewal is due: half of the validity has elapsed (whole seconds, as
/// [`Validity::renewal_due`] counts).
fn until_renewal(validity: Option<Validity>, now: u64) -> Duration {
    let Some(v) = validity else {
        return Duration::from_secs(1);
    };
    let half = v.not_before + v.not_after.saturating_sub(v.not_before) / 2;
    Duration::from_secs((half + 1).saturating_sub(now)).max(Duration::from_millis(100))
}

/// Checks that `pem` certifies `key` and chains to `root_der`, then builds the handle that
/// signs leaves.
fn accept(root_der: &[u8], key: KeyPair, pem: &str, now: u64) -> Result<Current, Failure> {
    let mismatch = |why: &str| Failure::Mismatch(why.to_string());
    let der = first_certificate(pem).ok_or_else(|| Failure::Error("no certificate".into()))?;
    let (_, cert) =
        X509Certificate::from_der(&der).map_err(|_| mismatch("unparsable certificate"))?;
    let (_, root) = X509Certificate::from_der(root_der).map_err(|_| mismatch("unparsable root"))?;
    if cert.public_key().raw != key.subject_public_key_info().as_slice() {
        return Err(mismatch("certificate is for another key"));
    }
    if cert.issuer().as_raw() != root.subject().as_raw() {
        return Err(mismatch("issuer is not the deployment root"));
    }
    if cert.verify_signature(Some(root.public_key())).is_err() {
        return Err(mismatch(
            "signature does not verify against the deployment root",
        ));
    }
    if !cert.is_ca() {
        return Err(mismatch("not a CA certificate"));
    }
    let validity = Validity {
        not_before: u64::try_from(cert.validity().not_before.timestamp()).unwrap_or(0),
        not_after: u64::try_from(cert.validity().not_after.timestamp()).unwrap_or(0),
    };
    if !validity.usable(now) {
        return Err(Failure::Error("certificate not within its validity".into()));
    }
    let der = CertificateDer::from(der);
    let issuer = Issuer::from_ca_cert_der(&der, key).map_err(|e| Failure::Error(e.to_string()))?;
    Ok(Current {
        validity,
        issuer,
        der,
    })
}
