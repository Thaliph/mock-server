//! État partagé par les listeners.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};

use http_body_util::combinators::UnsyncBoxBody;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use mimic_core::infer::{InferOptions, infer, infer_outbound};
use mimic_core::mock::{MockConfig, MockEngine};
use mimic_core::model::Direction;
use mimic_core::redact::{RedactConfig, Redactor};
use mimic_core::snapshot::Snapshot;
use serde_json::Value;

use crate::config::{Config, Mode};
use crate::metrics::{DiscoveryLabels, Metrics, OriginLabel};
use crate::store::Store;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type ProxyBody = UnsyncBoxBody<bytes::Bytes, BoxError>;

/// Documents OpenAPI calculés pour une version du store.
#[derive(Debug, Clone)]
pub struct Discovery {
    pub inbound: Value,
    /// Un document par dépendance, indexé par authority.
    pub outbound: BTreeMap<String, Value>,
}

pub struct TlsState {
    pub ca: Arc<crate::tls::Ca>,
    pub server: Arc<rustls::ServerConfig>,
}

pub struct AppState {
    pub config: Config,
    mode: AtomicU8,
    pub store: Store,
    pub metrics: Metrics,
    pub redactor: Redactor,
    pub client: Client<HttpsConnector<HttpConnector>, ProxyBody>,
    /// Interception TLS : CA et configuration serveur, si `tls.intercept` n'est pas vide.
    pub tls: Option<TlsState>,
    engine: Mutex<Option<(EngineKey, MockEngine)>>,
    /// Snapshot imposé par le contrôleur : seule source du mock quand il est présent.
    snapshot: Mutex<Option<Arc<Snapshot>>>,
    pub controller_status: Mutex<crate::control::ControllerStatus>,
    discovery: Mutex<Option<(u64, Arc<Discovery>)>>,
    pub share_status: Mutex<ShareStatus>,
    /// Réveille la boucle de partage (`POST /share/sync`).
    pub share_notify: tokio::sync::Notify,
    /// Adresse de l'application pour le trafic entrant, par port (spec §3.1).
    pub inbound_targets: crate::net::InboundTargets,
}

/// Source du moteur de mock en cache.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EngineKey {
    Store(u64),
    Snapshot(String),
}

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ShareStatus {
    pub enabled: bool,
    pub peers: Vec<String>,
    pub last_sync_ms: Option<u64>,
    pub last_error: Option<String>,
}

impl AppState {
    pub fn new(config: Config) -> Result<Arc<Self>, String> {
        let salt = match &config.redact.salt {
            Some(s) => s.as_bytes().to_vec(),
            None => ulid::Ulid::generate().to_string().into_bytes(),
        };
        let redactor = Redactor::new(
            RedactConfig {
                extra_json_paths: config.redact.extra_json_paths.clone(),
                ..RedactConfig::default()
            },
            salt,
        );
        let mut http = HttpConnector::new();
        http.set_nodelay(true);
        http.enforce_http(false);
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(crate::tls::client_config(
                config.tls.upstream_ca_file.as_deref(),
            )?)
            .https_or_http()
            .enable_http1()
            .wrap_connector(http);
        let client = Client::builder(TokioExecutor::new()).build(connector);
        let tls = if config.tls.intercept.is_empty() {
            None
        } else {
            let ca = Arc::new(crate::tls::Ca::load(&config.tls)?);
            if let Some(dir) = &config.tls.ca_dir {
                ca.publish(dir)?;
            }
            Some(TlsState {
                server: ca.server_config()?,
                ca,
            })
        };
        let metrics = Metrics::new();
        metrics.set_mode(config.mode);
        let store = Store::new(
            config.capture.max_exchanges,
            config.capture.samples_per_operation,
        );
        let share_status = ShareStatus {
            enabled: config.share.is_some(),
            ..ShareStatus::default()
        };
        let controller_url = config
            .share
            .as_ref()
            .filter(|s| s.is_controller())
            .map(|s| s.url.clone())
            .unwrap_or_default();
        Ok(Arc::new(Self {
            mode: AtomicU8::new(config.mode.to_u8()),
            inbound_targets: crate::net::InboundTargets::new(config.inbound.upstream),
            config,
            store,
            metrics,
            redactor,
            client,
            engine: Mutex::new(None),
            snapshot: Mutex::new(None),
            controller_status: Mutex::new(crate::control::ControllerStatus {
                url: controller_url,
                ..Default::default()
            }),
            discovery: Mutex::new(None),
            share_status: Mutex::new(share_status),
            share_notify: tokio::sync::Notify::new(),
            tls,
        }))
    }

    /// Vrai si l'hôte doit être intercepté en TLS.
    pub fn intercepts(&self, host: &str) -> bool {
        self.tls.is_some() && crate::tls::intercepted(&self.config.tls.intercept, host)
    }

    pub fn mode(&self) -> Mode {
        Mode::from_u8(self.mode.load(Ordering::SeqCst))
    }

    pub fn set_mode(&self, mode: Mode) {
        let previous = Mode::from_u8(self.mode.swap(mode.to_u8(), Ordering::SeqCst));
        if previous != mode {
            tracing::info!(
                from = previous.as_str(),
                to = mode.as_str(),
                "changement de mode"
            );
        }
        self.metrics.set_mode(mode);
    }

    pub fn snapshot(&self) -> Option<Arc<Snapshot>> {
        self.snapshot.lock().expect("verrou du snapshot").clone()
    }

    pub fn set_snapshot(&self, snap: Option<Arc<Snapshot>>) {
        self.metrics
            .mock_snapshot_exchanges
            .set(snap.as_ref().map_or(0, |s| s.exchanges.len() as i64));
        *self.snapshot.lock().expect("verrou du snapshot") = snap;
    }

    /// Exécute `f` avec le moteur de mock : construit sur le snapshot imposé s'il y en a un,
    /// sinon sur les derniers enregistrements sortants.
    pub fn with_engine<R>(&self, f: impl FnOnce(&mut MockEngine) -> R) -> R {
        let snapshot = self.snapshot();
        let key = match &snapshot {
            Some(s) => EngineKey::Snapshot(s.id.clone()),
            None => EngineKey::Store(self.store.outbound_version()),
        };
        let mut guard = self.engine.lock().expect("verrou du moteur de mock");
        let stale = guard.as_ref().is_none_or(|(k, _)| *k != key);
        if stale {
            let m = &self.config.mock;
            let exchanges = match &snapshot {
                Some(s) => s.exchanges.clone(),
                None => self.store.snapshot_direction(Direction::Outbound),
            };
            let engine = MockEngine::new(
                exchanges,
                MockConfig {
                    strategy: m.strategy.into(),
                    latency_factor: m.latency_factor,
                    match_headers: m.match_headers.clone(),
                    seed: m.seed,
                },
            );
            *guard = Some((key, engine));
        }
        let (_, engine) = guard.as_mut().expect("moteur initialisé");
        f(engine)
    }

    /// Documents OpenAPI du workload, recalculés quand le store change.
    pub fn discovery(&self) -> Arc<Discovery> {
        let version = self.store.version();
        let mut guard = self.discovery.lock().expect("verrou de la découverte");
        if let Some((v, d)) = guard.as_ref()
            && *v == version
        {
            return d.clone();
        }
        let exchanges = self.store.snapshot();
        let title = if self.config.workload.name.is_empty() {
            self.config.instance.clone()
        } else {
            self.config.workload.name.clone()
        };
        let version_label = env!("CARGO_PKG_VERSION");
        let d = Arc::new(Discovery {
            inbound: infer(&exchanges, &InferOptions::new(title, version_label)),
            outbound: infer_outbound(&exchanges, version_label),
        });
        self.update_discovery_metrics(&d);
        *guard = Some((version, d.clone()));
        d
    }

    fn update_discovery_metrics(&self, d: &Discovery) {
        let count = |doc: &Value| -> i64 {
            doc["paths"]
                .as_object()
                .map(|p| {
                    p.values()
                        .map(|item| item.as_object().map_or(0, |m| m.len()))
                        .sum::<usize>()
                })
                .unwrap_or(0) as i64
        };
        let inbound_authority = self
            .config
            .inbound
            .upstream
            .map(|a| a.to_string())
            .unwrap_or_else(|| "app".into());
        self.metrics.discovered_operations.clear();
        self.metrics
            .discovered_operations
            .get_or_create(&DiscoveryLabels {
                direction: "inbound".into(),
                authority: self.metrics.authority(&inbound_authority),
            })
            .set(count(&d.inbound));
        for (authority, doc) in &d.outbound {
            self.metrics
                .discovered_operations
                .get_or_create(&DiscoveryLabels {
                    direction: "outbound".into(),
                    authority: self.metrics.authority(authority),
                })
                .set(count(doc));
        }
    }

    pub fn refresh_store_metrics(&self) {
        let (local, peer) = self.store.counts();
        for (origin, n) in [("local", local), ("peer", peer)] {
            self.metrics
                .store_exchanges
                .get_or_create(&OriginLabel {
                    origin: origin.into(),
                })
                .set(n as i64);
        }
    }
}
