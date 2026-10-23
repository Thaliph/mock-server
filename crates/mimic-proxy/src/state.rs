//! État partagé par les listeners.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use http_body_util::combinators::UnsyncBoxBody;
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use mimic_core::infer::{InferOptions, infer, infer_outbound};
use mimic_core::redact::{RedactConfig, Redactor};
use mimic_core::snapshot::MockSnapshot;
use mimic_core::unmatched::{Reason, Unmatched, UnmatchedLog, UnmatchedReport};
use mimic_core::startup::{Gate, Phase};
use serde_json::Value;

use crate::config::{Config, InboundCapture, MockWebsocket, Mode};
use crate::connect::TimeoutConnector;
use crate::metrics::{DiscoveryLabels, Metrics, OriginLabel};
use crate::store::{Drops, Limits, Store};
use crate::throttle::LogLimiter;

pub type BoxError = Box<dyn std::error::Error + Send + Sync>;
pub type ProxyBody = UnsyncBoxBody<bytes::Bytes, BoxError>;
/// Connecteur vers l'amont, borné par `outbound.connect_timeout` (spec §3.5).
pub type Upstream = TimeoutConnector<HttpsConnector<HttpConnector>>;

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
    pub client: Client<Upstream, ProxyBody>,
    /// Client HTTP/2 seulement (h2c en connaissance préalable, h2 via ALPN) : gRPC.
    pub client_h2: Client<Upstream, ProxyBody>,
    /// Interception TLS : CA et configuration serveur, si `tls.intercept` n'est pas vide.
    pub tls: Option<TlsState>,
    /// Moteur de mock en service (spec §5.3).
    pub(crate) engines: crate::engine::Engines,
    /// Forme de mock du snapshot imposé par le contrôleur (spec §5.6) : seule source du mock
    /// quand elle est présente.
    pub(crate) snapshot: Mutex<Option<Arc<MockSnapshot>>>,
    pub controller_status: Mutex<crate::control::ControllerStatus>,
    /// Barrière de démarrage sous contrôleur (spec §5.5.5). Ses transitions et l'application
    /// de l'état souhaité se font sous ce verrou : l'échéance du délai et un battement ne
    /// s'entrecroisent pas.
    pub(crate) startup: tokio::sync::Mutex<Gate>,
    /// État de la barrière, lu sans verrou par `/readyz`.
    startup_phase: AtomicU8,
    discovery: Mutex<Option<(u64, Arc<Discovery>)>>,
    /// Pris pendant un calcul de la découverte : un seul à la fois, les autres l'attendent.
    discovery_build: Mutex<()>,
    pub share_status: Mutex<ShareStatus>,
    /// Réveille la boucle de partage (`POST /share/sync`).
    pub share_notify: tokio::sync::Notify,
    /// Adresse de l'application pour le trafic entrant, par port (spec §3.1).
    pub inbound_targets: crate::net::InboundTargets,
    /// Logs d'échec vers l'amont, limités en débit (spec §9.3).
    pub upstream_log: LogLimiter,
    /// Places de connexion de l'entrant et du sortant ensemble (`server.max_connections`).
    pub(crate) connections: Arc<tokio::sync::Semaphore>,
    /// Places de connexion de l'admin, à part : l'application ne prive pas les sondes.
    pub(crate) admin_connections: Arc<tokio::sync::Semaphore>,
    /// Budget des bodies copiés en vol par la capture (`capture.max_inflight_bytes`).
    pub(crate) inflight: Arc<crate::capture::InflightBudget>,
    /// Logs de refus au plafond de connexions, limités en débit.
    pub(crate) refused_log: LogLimiter,
    /// Logs des connexions refusées en mock (TLS non intercepté, TCP non HTTP), limités en
    /// débit (spec §3.1).
    pub(crate) tunnel_refused_log: LogLimiter,
    /// Requêtes refusées par le mock (`599`, `413`) du tir en cours (spec §5.3).
    unmatched: Mutex<UnmatchedLog>,
    /// Logs de ces refus, limités en débit, un par raison (spec §9.3).
    unmatched_log: LogLimiter,
    too_large_log: LogLimiter,
}

/// Plafond fixe des connexions simultanées de l'admin (spec §3.5).
pub const ADMIN_MAX_CONNECTIONS: usize = 64;

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
            None => {
                // Sel propre au processus : les marqueurs ne se comparent plus entre pods (§7).
                tracing::warn!(
                    "aucun sel de redaction (redact.salt, MIMIC_REDACT_SALT) : sel aléatoire, \
                     les marqueurs de redaction changent d'un pod et d'un redémarrage à l'autre \
                     (dédoublonnage, snapshots et comparaisons entre pods faussés) ; fournir \
                     MIMIC_REDACT_SALT par le Secret <workload>-mimic, clé redact-salt"
                );
                ulid::Ulid::generate().to_string().into_bytes()
            }
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
        let connect_timeout = config.outbound.connect_timeout;
        let client = Client::builder(TokioExecutor::new())
            .build(TimeoutConnector::new(connector, connect_timeout));
        let connector_h2 = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config(tls_client)
            .https_or_http()
            .enable_http2()
            .wrap_connector(http);
        let client_h2 = Client::builder(TokioExecutor::new())
            .http2_only(true)
            .build(TimeoutConnector::new(connector_h2, connect_timeout));
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
        // Hors contrôleur, aucun état souhaité à attendre : barrière ouverte d'emblée.
        let startup_phase = if config.controlled() {
            metrics.set_startup(Phase::Waiting);
            Phase::Waiting
        } else {
            Phase::Aligned
        };
        let fallback = config
            .share
            .as_ref()
            .map(|s| s.startup.fallback)
            .unwrap_or_default();
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
        let max_connections = config.server.max_connections;
        let inflight = crate::capture::InflightBudget::new(config.capture.max_inflight_bytes);
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
            startup: tokio::sync::Mutex::new(Gate::new(fallback)),
            startup_phase: AtomicU8::new(startup_phase.to_u8()),
            discovery: Mutex::new(None),
            discovery_build: Mutex::new(()),
            share_status: Mutex::new(share_status),
            share_notify: tokio::sync::Notify::new(),
            upstream_log: LogLimiter::per_second(),
            connections: Arc::new(tokio::sync::Semaphore::new(max_connections)),
            admin_connections: Arc::new(tokio::sync::Semaphore::new(ADMIN_MAX_CONNECTIONS)),
            refused_log: LogLimiter::per_second(),
            tunnel_refused_log: LogLimiter::per_second(),
            unmatched: Mutex::new(UnmatchedLog::default()),
            unmatched_log: LogLimiter::per_second(),
            too_large_log: LogLimiter::per_second(),
            inflight,
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
        } else if !crate::engine::mock_mode(previous) {
            // Nouveau tir : le relevé des requêtes sans réponse repart de zéro (spec §5.3).
            self.unmatched_lock().clear();
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

    /// État de la barrière de démarrage (spec §5.5.5).
    pub fn startup_phase(&self) -> Phase {
        Phase::from_u8(self.startup_phase.load(Ordering::SeqCst))
    }

    /// Publie l'état de la barrière, tenue sous le verrou `startup`.
    pub(crate) fn publish_startup(&self, gate: &Gate) {
        let phase = gate.phase();
        self.startup_phase.store(phase.to_u8(), Ordering::SeqCst);
        self.metrics.set_startup(phase);
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
        self.snapshot
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Impose un snapshot (ou le retire) ; en mock, le moteur est reconstruit dessus avant
    /// de servir.
    pub fn set_snapshot(&self, snap: Option<Arc<MockSnapshot>>) {
        if let Some(new) = &snap
            && self.snapshot().is_none_or(|old| old.id != new.id)
        {
            // Autre jeu de données : les requêtes sans réponse d'avant ne disent plus rien.
            self.unmatched_lock().clear();
        }
        self.metrics
            .mock_snapshot_exchanges
            .set(snap.as_ref().map_or(0, |s| s.count as i64));
        self.publish_snapshot(snap);
    }

    fn unmatched_lock(&self) -> std::sync::MutexGuard<'_, UnmatchedLog> {
        self.unmatched
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Relevé des requêtes sans réponse, au plus `limit` clés (spec §5.3).
    pub fn unmatched_report(&self, limit: usize) -> UnmatchedReport {
        self.unmatched_lock().report(limit)
    }

    /// Requête refusée par le mock : relevée et journalisée en débit limité, avec la forme
    /// de son chemin seulement (spec §5.3, §9.3).
    pub(crate) fn note_unmatched(&self, u: &Unmatched, trace_id: Option<&str>) {
        self.unmatched_lock().record(u, crate::share::now_ms());
        let (limiter, message) = match u.reason {
            Reason::Unmatched => (
                &self.unmatched_log,
                "mock : requête sans réponse enregistrée",
            ),
            Reason::TooLarge => (&self.too_large_log, "mock : body de requête trop gros"),
        };
        if let Some(omitted) = limiter.allow(std::time::Instant::now()) {
            tracing::warn!(
                method = %u.method,
                authority = %u.authority,
                path = %u.path,
                trace_id = trace_id.unwrap_or_default(),
                reason = u.reason.as_str(),
                status = u.reason.status(),
                omitted,
                "{message}"
            );
        }
    }

    /// Documents OpenAPI du workload, recalculés quand le store change, sur le pool de
    /// threads bloquants : un calcul long ne retient aucun worker du runtime (spec §9.2).
    pub async fn discovery(self: &Arc<Self>) -> Arc<Discovery> {
        if let Some(d) = self.cached_discovery(self.store.version()) {
            return d;
        }
        let state = self.clone();
        let compute = move || state.compute_discovery();
        match tokio::task::spawn_blocking(compute.clone()).await {
            Ok(d) => d,
            // Pool bloquant indisponible (arrêt du runtime) : calcul sur place.
            Err(_) => compute(),
        }
    }

    /// Découverte en cache, si elle correspond à cette version du store.
    fn cached_discovery(&self, version: u64) -> Option<Arc<Discovery>> {
        let guard = self
            .discovery
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        guard
            .as_ref()
            .filter(|(v, _)| *v == version)
            .map(|(_, d)| d.clone())
    }

    /// Calcul de la découverte, hors du verrou du cache, sur des échanges partagés avec le
    /// store (aucune copie sous son verrou).
    fn compute_discovery(&self) -> Arc<Discovery> {
        let _build = self
            .discovery_build
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        // Version lue avant les échanges : un ajout pendant le calcul le fera refaire.
        let version = self.store.version();
        if let Some(d) = self.cached_discovery(version) {
            return d;
        }
        let exchanges = self.store.shared();
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
        *self
            .discovery
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some((version, d.clone()));
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
