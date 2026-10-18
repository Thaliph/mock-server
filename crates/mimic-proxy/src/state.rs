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
use mimic_core::redact::{RedactConfig, Redactor};
use mimic_core::snapshot::MockSnapshot;
use serde_json::Value;

use crate::config::{Config, InboundCapture, MockWebsocket, Mode};
use crate::metrics::{DiscoveryLabels, Metrics, OriginLabel};
use crate::store::{Drops, Limits, Store};
use crate::throttle::LogLimiter;

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
    /// Capture de l'entrant courante : celle de la configuration, ou celle du contrôleur.
    inbound_capture: AtomicU8,
    /// WebSocket sortant en mock : réglage de la configuration, ou celui du contrôleur.
    mock_websocket: AtomicU8,
    pub store: Store,
    pub metrics: Metrics,
    pub redactor: Redactor,
    pub client: Client<HttpsConnector<HttpConnector>, ProxyBody>,
    /// Client HTTP/2 seulement (h2c en connaissance préalable, h2 via ALPN) : gRPC.
    pub client_h2: Client<HttpsConnector<HttpConnector>, ProxyBody>,
    /// Interception TLS : CA et configuration serveur, si `tls.intercept` n'est pas vide.
    pub tls: Option<TlsState>,
    /// Moteur de mock en service (spec §5.3).
    pub(crate) engines: crate::engine::Engines,
    /// Forme de mock du snapshot imposé par le contrôleur (spec §5.6) : seule source du mock
    /// quand elle est présente.
    pub(crate) snapshot: Mutex<Option<Arc<MockSnapshot>>>,
    pub controller_status: Mutex<crate::control::ControllerStatus>,
    discovery: Mutex<Option<(u64, Arc<Discovery>)>>,
    pub share_status: Mutex<ShareStatus>,
    /// Réveille la boucle de partage (`POST /share/sync`).
    pub share_notify: tokio::sync::Notify,
    /// Adresse de l'application pour le trafic entrant, par port (spec §3.1).
    pub inbound_targets: crate::net::InboundTargets,
    /// Logs d'échec vers l'amont, limités en débit (spec §9.3).
    pub upstream_log: LogLimiter,
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
        let tls_client = crate::tls::client_config(config.tls.upstream_ca_file.as_deref())?;
        let connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls_client.clone())
            .https_or_http()
            .enable_http1()
            .wrap_connector(http.clone());
        let client = Client::builder(TokioExecutor::new()).build(connector);
        let connector_h2 = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls_client)
            .https_or_http()
            .enable_http2()
            .wrap_connector(http);
        let client_h2 = Client::builder(TokioExecutor::new())
            .http2_only(true)
            .build(connector_h2);
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
        let metrics = Metrics::with_version(config.metrics_version());
        metrics.set_mode(config.mode);
        let store = Store::with_limits(
            Limits {
                max_exchanges: config.capture.max_exchanges,
                samples: config.capture.samples_per_operation,
                max_bytes: config.capture.max_store_bytes,
                share_backlog: config.share.as_ref().map(|s| s.max_backlog_bytes),
                share_pending: config.share.as_ref().map_or(usize::MAX, |s| s.max_pending),
            },
            Drops {
                store_budget: metrics.dropped("store_budget"),
                share_backlog: metrics.dropped("share_backlog"),
                local_backlog: metrics.dropped("local_backlog"),
            },
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
            inbound_capture: AtomicU8::new(config.capture.inbound as u8),
            mock_websocket: AtomicU8::new(config.mock.websocket as u8),
            inbound_targets: crate::net::InboundTargets::new(config.inbound.upstream),
            config,
            store,
            metrics,
            redactor,
            client,
            client_h2,
            engines: crate::engine::Engines::default(),
            snapshot: Mutex::new(None),
            controller_status: Mutex::new(crate::control::ControllerStatus {
                url: controller_url,
                ..Default::default()
            }),
            discovery: Mutex::new(None),
            share_status: Mutex::new(share_status),
            share_notify: tokio::sync::Notify::new(),
            upstream_log: LogLimiter::per_second(),
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

    /// Change de mode. Entrer en mock construit d'abord le moteur, de sorte que la première
    /// requête mockée le trouve prêt ; quitter le mock le libère (spec §5.3).
    pub fn set_mode(&self, mode: Mode) {
        let mock = crate::engine::mock_mode(mode);
        if mock {
            self.prepare_mock();
        }
        let previous = Mode::from_u8(self.mode.swap(mode.to_u8(), Ordering::SeqCst));
        if !mock {
            self.release_mock();
        }
        if previous != mode {
            tracing::info!(
                from = previous.as_str(),
                to = mode.as_str(),
                "changement de mode"
            );
        }
        self.metrics.set_mode(mode);
    }

    pub fn inbound_capture(&self) -> InboundCapture {
        InboundCapture::from_u8(self.inbound_capture.load(Ordering::SeqCst))
    }

    /// Change la capture de l'entrant, pour les requêtes suivantes.
    pub fn set_inbound_capture(&self, c: InboundCapture) {
        let previous =
            InboundCapture::from_u8(self.inbound_capture.swap(c as u8, Ordering::SeqCst));
        if previous != c {
            tracing::info!(
                from = previous.as_str(),
                to = c.as_str(),
                "capture de l'entrant"
            );
        }
    }

    pub fn mock_websocket(&self) -> MockWebsocket {
        MockWebsocket::from_u8(self.mock_websocket.load(Ordering::SeqCst))
    }

    /// Change le traitement des WebSockets sortants en mock, pour les poignées de main
    /// suivantes ; les connexions déjà relayées continuent.
    pub fn set_mock_websocket(&self, w: MockWebsocket) {
        let previous = MockWebsocket::from_u8(self.mock_websocket.swap(w as u8, Ordering::SeqCst));
        if previous != w {
            tracing::info!(
                from = previous.as_str(),
                to = w.as_str(),
                "WebSocket sortant en mock"
            );
        }
    }

    pub fn snapshot(&self) -> Option<Arc<MockSnapshot>> {
        self.snapshot.lock().expect("verrou du snapshot").clone()
    }

    /// Impose un snapshot (ou le retire) ; en mock, le moteur est reconstruit dessus avant
    /// de servir.
    pub fn set_snapshot(&self, snap: Option<Arc<MockSnapshot>>) {
        self.metrics
            .mock_snapshot_exchanges
            .set(snap.as_ref().map_or(0, |s| s.count as i64));
        self.publish_snapshot(snap);
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
                direction: "inbound",
                authority: self.metrics.authority(&inbound_authority),
            })
            .set(count(&d.inbound));
        for (authority, doc) in &d.outbound {
            self.metrics
                .discovered_operations
                .get_or_create(&DiscoveryLabels {
                    direction: "outbound",
                    authority: self.metrics.authority(authority),
                })
                .set(count(doc));
        }
    }

    pub fn refresh_store_metrics(&self) {
        self.metrics.store_bytes.set(self.store.bytes() as i64);
        let (local, peer) = self.store.counts();
        for (origin, n) in [("local", local), ("peer", peer)] {
            self.metrics
                .store_exchanges
                .get_or_create(&OriginLabel { origin })
                .set(n as i64);
        }
    }
}
