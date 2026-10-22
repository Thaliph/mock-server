//! Métriques Prometheus (spec §9.3).
//!
//! Compter une requête n'alloue rien et ne touche aucun compteur de références : les labels
//! sont des chaînes statiques, le code un entier. Une authority est internée en chaîne statique
//! à sa première apparition (au plus `MAX_AUTHORITIES`, la mémoire reste bornée), puis résolue
//! sous un verrou pris en lecture. Seule la première requête d'une série la crée.

use std::collections::BTreeSet;
use std::sync::RwLock;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use mimic_core::model::Direction;

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::{Histogram, exponential_buckets};
use prometheus_client::registry::Registry;

use crate::config::Mode;

/// Au-delà de ce nombre d'authorities distinctes, les suivantes sont regroupées sous `other`.
const MAX_AUTHORITIES: usize = 100;

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct RequestLabels {
    pub direction: &'static str,
    pub mode: &'static str,
    pub authority: &'static str,
    /// Méthode standard, sinon `other` (spec §9.3).
    pub method: &'static str,
    pub status: u16,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct DurationLabels {
    pub direction: &'static str,
    pub mode: &'static str,
    pub authority: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct UpstreamErrorLabels {
    pub direction: &'static str,
    pub authority: &'static str,
    pub kind: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct MockLabels {
    pub authority: &'static str,
    pub r#match: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct AuthorityLabel {
    pub authority: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct DiscoveryLabels {
    pub direction: &'static str,
    pub authority: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct OriginLabel {
    pub origin: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ReasonLabel {
    pub reason: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ResultLabel {
    pub result: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct PassthroughLabels {
    pub direction: &'static str,
    pub kind: &'static str,
    pub result: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ListenerLabel {
    /// `inbound`, `outbound` ou `admin`.
    pub listener: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct TlsFailureLabels {
    pub authority: &'static str,
    pub reason: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct TriggerLabel {
    pub trigger: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct WebsocketLabels {
    pub direction: &'static str,
    /// `relayed` (`101` de l'amont relayé) ou `refused` (refus en mock, spec §3.4).
    pub result: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ModeLabel {
    pub mode: &'static str,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct StateLabel {
    /// État de la barrière de démarrage (spec §5.5.5).
    pub state: &'static str,
}

type HistogramFamily<L> = Family<L, Histogram, fn() -> Histogram>;

fn latency_histogram() -> Histogram {
    // De 0,5 ms à environ 16 s.
    Histogram::new(exponential_buckets(0.0005, 2.0, 16))
}

pub struct Metrics {
    registry: Registry,
    pub requests: Family<RequestLabels, Counter>,
    pub request_duration: HistogramFamily<DurationLabels>,
    pub upstream_errors: Family<UpstreamErrorLabels, Counter>,
    pub mock_responses: Family<MockLabels, Counter>,
    pub mock_latency: HistogramFamily<AuthorityLabel>,
    pub discovered_operations: Family<DiscoveryLabels, Gauge>,
    pub store_exchanges: Family<OriginLabel, Gauge>,
    pub store_bytes: Gauge,
    pub capture_dropped: Family<ReasonLabel, Counter>,
    pub share_sync: Family<ResultLabel, Counter>,
    pub share_peers: Gauge,
    pub mock_snapshot_exchanges: Gauge,
    pub mock_engine_builds: Family<TriggerLabel, Counter>,
    pub mode: Family<ModeLabel, Gauge>,
    pub passthrough: Family<PassthroughLabels, Counter>,
    pub tls_handshake_failures: Family<TlsFailureLabels, Counter>,
    pub websocket_connections: Family<WebsocketLabels, Counter>,
    pub connections_refused: Family<ListenerLabel, Counter>,
    /// Battements vers le contrôleur, par résultat (`ok`, `error`).
    pub heartbeats: Family<ResultLabel, Counter>,
    /// Date Unix, en secondes, du dernier battement auquel le contrôleur a répondu.
    pub heartbeat_last_success: Gauge<f64, AtomicU64>,
    /// État de la barrière de démarrage : 1 pour l'état courant (spec §5.5.5).
    pub startup: Family<StateLabel, Gauge>,
    /// Authorities internées : au plus `MAX_AUTHORITIES` chaînes, jamais libérées.
    authorities: RwLock<BTreeSet<&'static str>>,
}

impl Metrics {
    pub fn new() -> Self {
        Self::with_version(None)
    }

    /// Métriques dont chaque série porte le label constant `version`, s'il est donné
    /// (spec §9.3) : une seule valeur par pod, aucune série en plus.
    pub fn with_version(version: Option<String>) -> Self {
        let mut registry = match version {
            Some(v) => Registry::with_labels(
                [(
                    std::borrow::Cow::Borrowed("version"),
                    std::borrow::Cow::Owned(v),
                )]
                .into_iter(),
            ),
            None => Registry::default(),
        };
        let requests = Family::default();
        let request_duration: HistogramFamily<DurationLabels> =
            Family::new_with_constructor(latency_histogram);
        let upstream_errors = Family::default();
        let mock_responses = Family::default();
        let mock_latency: HistogramFamily<AuthorityLabel> =
            Family::new_with_constructor(latency_histogram);
        let discovered_operations = Family::default();
        let store_exchanges = Family::default();
        let store_bytes = Gauge::default();
        let capture_dropped = Family::default();
        let share_sync = Family::default();
        let share_peers = Gauge::default();
        let mock_snapshot_exchanges = Gauge::default();
        let mock_engine_builds = Family::default();
        let mode = Family::default();
        let passthrough = Family::default();
        let tls_handshake_failures = Family::default();
        let websocket_connections = Family::default();
        let connections_refused = Family::default();
        let heartbeats = Family::default();
        let heartbeat_last_success = Gauge::<f64, AtomicU64>::default();
        let startup = Family::default();
        registry.register(
            "mimic_requests",
            "Requêtes traitées par le sidecar",
            requests.clone(),
        );
        registry.register(
            "mimic_request_duration_seconds",
            "Durée des requêtes vue par l'appelant, en secondes",
            request_duration.clone(),
        );
        registry.register(
            "mimic_upstream_errors",
            "Échecs de connexion ou de réponse de l'amont",
            upstream_errors.clone(),
        );
        registry.register(
            "mimic_mock_responses",
            "Réponses servies en mode mock, par type de correspondance",
            mock_responses.clone(),
        );
        registry.register(
            "mimic_mock_injected_latency_seconds",
            "Latence injectée par le mode mock, en secondes",
            mock_latency.clone(),
        );
        registry.register(
            "mimic_discovered_operations",
            "Opérations découvertes",
            discovered_operations.clone(),
        );
        registry.register(
            "mimic_store_exchanges",
            "Échanges en mémoire",
            store_exchanges.clone(),
        );
        registry.register(
            "mimic_store_bytes",
            "Octets estimés des échanges en mémoire",
            store_bytes.clone(),
        );
        registry.register(
            "mimic_capture_dropped",
            "Captures abandonnées ou réduites",
            capture_dropped.clone(),
        );
        registry.register(
            "mimic_share_sync",
            "Synchronisations avec les autres sidecars du workload",
            share_sync.clone(),
        );
        registry.register(
            "mimic_share_peers",
            "Autres sidecars du workload vus au dernier partage",
            share_peers.clone(),
        );
        registry.register(
            "mimic_mock_snapshot_exchanges",
            "Échanges du snapshot imposé par le contrôleur (0 sans snapshot)",
            mock_snapshot_exchanges.clone(),
        );
        registry.register(
            "mimic_mock_engine_builds",
            "Constructions du moteur de mock, par déclencheur (switch, refresh, unmatched)",
            mock_engine_builds.clone(),
        );
        registry.register("mimic_mode", "Mode courant (1)", mode.clone());
        registry.register(
            "mimic_passthrough_connections",
            "Connexions redirigées non HTTP relayées en tunnel",
            passthrough.clone(),
        );
        registry.register(
            "mimic_tls_handshake_failures",
            "Poignées de main TLS interceptées refusées (CA non approuvée, épinglage)",
            tls_handshake_failures.clone(),
        );
        registry.register(
            "mimic_websocket_connections",
            "Poignées de main WebSocket : 101 de l'amont relayé, ou refus en mock",
            websocket_connections.clone(),
        );
        registry.register(
            "mimic_connections_refused",
            "Connexions refermées au plafond de connexions simultanées (spec §3.5)",
            connections_refused.clone(),
        );
        registry.register(
            "mimic_heartbeats",
            "Battements vers le contrôleur, par résultat (ok : état souhaité reçu)",
            heartbeats.clone(),
        );
        registry.register(
            "mimic_heartbeat_last_success_timestamp_seconds",
            "Date Unix du dernier battement auquel le contrôleur a répondu (0 avant le premier)",
            heartbeat_last_success.clone(),
        );
        registry.register(
            "mimic_startup",
            "État de la barrière de démarrage sous contrôleur (1 pour l'état courant)",
            startup.clone(),
        );
        Self {
            registry,
            requests,
            request_duration,
            upstream_errors,
            mock_responses,
            mock_latency,
            discovered_operations,
            store_exchanges,
            store_bytes,
            capture_dropped,
            share_sync,
            share_peers,
            mock_snapshot_exchanges,
            mock_engine_builds,
            mode,
            passthrough,
            tls_handshake_failures,
            websocket_connections,
            connections_refused,
            heartbeats,
            heartbeat_last_success,
            startup,
            authorities: RwLock::new(BTreeSet::new()),
        }
    }

    /// Valeur de label d'authority à cardinalité bornée. Une authority déjà vue est lue sous le
    /// verrou pris en lecture, sans copie ; une nouvelle est internée, dans la limite de
    /// `MAX_AUTHORITIES`.
    pub fn authority(&self, authority: &str) -> &'static str {
        {
            let seen = self.authorities.read().unwrap_or_else(|e| e.into_inner());
            if let Some(a) = seen.get(authority) {
                return a;
            }
            if seen.len() >= MAX_AUTHORITIES {
                return "other";
            }
        }
        let mut seen = self.authorities.write().unwrap_or_else(|e| e.into_inner());
        if let Some(a) = seen.get(authority) {
            return a;
        }
        if seen.len() >= MAX_AUTHORITIES {
            return "other";
        }
        // Internée : bornée par `MAX_AUTHORITIES`, gardée toute la vie du processus.
        let a: &'static str = Box::leak(authority.to_string().into_boxed_str());
        seen.insert(a);
        a
    }

    /// Compteur `mimic_capture_dropped_total` d'une raison.
    pub fn dropped(&self, reason: &'static str) -> Counter {
        self.capture_dropped
            .get_or_create(&ReasonLabel { reason })
            .clone()
    }

    /// `mimic_requests_total` : une requête traitée.
    pub fn count_request(
        &self,
        direction: Direction,
        mode: Mode,
        authority: &'static str,
        method: &str,
        status: u16,
    ) {
        self.requests
            .get_or_create(&RequestLabels {
                direction: direction_label(direction),
                mode: mode.as_str(),
                authority,
                method: method_label(method),
                status,
            })
            .inc();
    }

    /// `mimic_request_duration_seconds` : durée vue par l'appelant.
    pub fn observe_duration(
        &self,
        direction: Direction,
        mode: Mode,
        authority: &'static str,
        d: Duration,
    ) {
        self.request_duration
            .get_or_create(&DurationLabels {
                direction: direction_label(direction),
                mode: mode.as_str(),
                authority,
            })
            .observe(d.as_secs_f64());
    }

    /// `mimic_mock_responses_total` : réponse du mock, par type de correspondance.
    pub fn count_mock(&self, authority: &'static str, kind: &'static str) {
        self.mock_responses
            .get_or_create(&MockLabels {
                authority,
                r#match: kind,
            })
            .inc();
    }

    /// `mimic_mock_injected_latency_seconds` : latence injectée par le mock.
    pub fn observe_mock_latency(&self, authority: &'static str, d: Duration) {
        self.mock_latency
            .get_or_create(&AuthorityLabel { authority })
            .observe(d.as_secs_f64());
    }

    /// `mimic_websocket_connections_total` : poignée de main relayée (`relayed`) ou refusée
    /// en mock (`refused`).
    pub fn count_websocket(&self, direction: Direction, result: &'static str) {
        self.websocket_connections
            .get_or_create(&WebsocketLabels {
                direction: direction_label(direction),
                result,
            })
            .inc();
    }

    /// `mimic_upstream_errors_total` : échec de connexion (`connect`) ou de réponse (`request`).
    pub fn count_upstream_error(
        &self,
        direction: Direction,
        authority: &'static str,
        kind: &'static str,
    ) {
        self.upstream_errors
            .get_or_create(&UpstreamErrorLabels {
                direction: direction_label(direction),
                authority,
                kind,
            })
            .inc();
    }

    /// `mimic_capture_dropped_total` : une capture abandonnée ou réduite.
    pub fn count_dropped(&self, reason: &'static str) {
        self.capture_dropped
            .get_or_create(&ReasonLabel { reason })
            .inc();
    }

    pub fn set_mode(&self, current: Mode) {
        for m in Mode::ALL {
            self.mode
                .get_or_create(&ModeLabel { mode: m.as_str() })
                .set(i64::from(m == current));
        }
    }

    /// `mimic_heartbeats_total` et, pour un succès, la date du dernier.
    pub fn count_heartbeat(&self, ok: bool, unix_secs: f64) {
        let result = if ok { "ok" } else { "error" };
        self.heartbeats.get_or_create(&ResultLabel { result }).inc();
        if ok {
            self.heartbeat_last_success.set(unix_secs);
        }
    }

    /// `mimic_startup{state}` : 1 pour l'état courant de la barrière, 0 pour les autres.
    pub fn set_startup(&self, current: mimic_core::startup::Phase) {
        for p in mimic_core::startup::Phase::ALL {
            self.startup
                .get_or_create(&StateLabel { state: p.as_str() })
                .set(i64::from(p == current));
        }
    }

    pub fn encode(&self) -> String {
        let mut out = String::new();
        prometheus_client::encoding::text::encode(&mut out, &self.registry)
            .expect("encodage des métriques");
        out
    }
}

/// Label `direction`.
pub fn direction_label(d: Direction) -> &'static str {
    match d {
        Direction::Inbound => "inbound",
        Direction::Outbound => "outbound",
    }
}

/// Label `method` : une des méthodes standard, sinon `other`, pour que la cardinalité ne
/// dépende pas des clients (spec §9.3).
fn method_label(method: &str) -> &'static str {
    const STANDARD: [&str; 9] = [
        "GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS", "CONNECT", "TRACE",
    ];
    STANDARD
        .iter()
        .find(|m| **m == method)
        .copied()
        .unwrap_or("other")
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}
