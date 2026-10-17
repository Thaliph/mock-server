//! Métriques Prometheus (spec §9.3).

use std::collections::BTreeSet;
use std::sync::Mutex;

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
    pub direction: String,
    pub mode: String,
    pub authority: String,
    pub method: String,
    pub status: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct DurationLabels {
    pub direction: String,
    pub mode: String,
    pub authority: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct UpstreamErrorLabels {
    pub direction: String,
    pub authority: String,
    pub kind: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct MockLabels {
    pub authority: String,
    pub r#match: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct AuthorityLabel {
    pub authority: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct DiscoveryLabels {
    pub direction: String,
    pub authority: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct OriginLabel {
    pub origin: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ReasonLabel {
    pub reason: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ResultLabel {
    pub result: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct PassthroughLabels {
    pub direction: String,
    pub kind: String,
    pub result: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct TlsFailureLabels {
    pub authority: String,
    pub reason: String,
}

#[derive(Clone, Debug, Hash, PartialEq, Eq, EncodeLabelSet)]
pub struct ModeLabel {
    pub mode: String,
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
    pub capture_dropped: Family<ReasonLabel, Counter>,
    pub share_sync: Family<ResultLabel, Counter>,
    pub share_peers: Gauge,
    pub mode: Family<ModeLabel, Gauge>,
    pub passthrough: Family<PassthroughLabels, Counter>,
    pub tls_handshake_failures: Family<TlsFailureLabels, Counter>,
    authorities: Mutex<BTreeSet<String>>,
}

impl Metrics {
    pub fn new() -> Self {
        let mut registry = Registry::default();
        let requests = Family::default();
        let request_duration: HistogramFamily<DurationLabels> =
            Family::new_with_constructor(latency_histogram);
        let upstream_errors = Family::default();
        let mock_responses = Family::default();
        let mock_latency: HistogramFamily<AuthorityLabel> =
            Family::new_with_constructor(latency_histogram);
        let discovered_operations = Family::default();
        let store_exchanges = Family::default();
        let capture_dropped = Family::default();
        let share_sync = Family::default();
        let share_peers = Gauge::default();
        let mode = Family::default();
        let passthrough = Family::default();
        let tls_handshake_failures = Family::default();
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
        Self {
            registry,
            requests,
            request_duration,
            upstream_errors,
            mock_responses,
            mock_latency,
            discovered_operations,
            store_exchanges,
            capture_dropped,
            share_sync,
            share_peers,
            mode,
            passthrough,
            tls_handshake_failures,
            authorities: Mutex::new(BTreeSet::new()),
        }
    }

    /// Valeur de label d'authority à cardinalité bornée.
    pub fn authority(&self, authority: &str) -> String {
        let mut seen = self.authorities.lock().expect("verrou des authorities");
        if seen.contains(authority) {
            return authority.to_string();
        }
        if seen.len() >= MAX_AUTHORITIES {
            return "other".to_string();
        }
        seen.insert(authority.to_string());
        authority.to_string()
    }

    pub fn set_mode(&self, current: Mode) {
        for m in Mode::ALL {
            self.mode
                .get_or_create(&ModeLabel {
                    mode: m.as_str().into(),
                })
                .set(i64::from(m == current));
        }
    }

    pub fn encode(&self) -> String {
        let mut out = String::new();
        prometheus_client::encoding::text::encode(&mut out, &self.registry)
            .expect("encodage des métriques");
        out
    }
}

impl Default for Metrics {
    fn default() -> Self {
        Self::new()
    }
}
