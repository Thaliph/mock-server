//! `mimic_discovery_*` metrics (spec §9).

use mimic_runtime::metrics::Metrics;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Unit;

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct ExchangeLabels {
    direction: &'static str,
    result: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct BodyLabels {
    direction: &'static str,
    part: &'static str,
    result: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct SampleLabels {
    direction: &'static str,
    result: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct RedactionLabels {
    location: &'static str,
    rule: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct DirectionLabels {
    direction: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct ResultLabel {
    result: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct TunnelLabels {
    kind: &'static str,
    result: &'static str,
}

/// Shared by the taps (which count what they drop) and the recorder. Unregistered until
/// [`DiscoveryMetrics::register`].
#[derive(Clone, Default)]
pub struct DiscoveryMetrics {
    exchanges: Family<ExchangeLabels, Counter>,
    bodies: Family<BodyLabels, Counter>,
    samples: Family<SampleLabels, Counter>,
    redactions: Family<RedactionLabels, Counter>,
    endpoints: Family<DirectionLabels, Gauge>,
    last_new: Family<DirectionLabels, Gauge>,
    authorities: Gauge,
    tunnels: Family<TunnelLabels, Counter>,
    sample_bytes: Gauge,
    pushes: Family<ResultLabel, Counter>,
    outbound_dropped: Counter,
    last_push: Gauge,
}

impl DiscoveryMetrics {
    pub fn register(&self, metrics: &mut Metrics) {
        // Every series exists from the start, at 0.
        for result in [
            "ok",
            "duplicate",
            "too_large",
            "rejected",
            "error",
            "inbound_only",
        ] {
            let _ = self.pushes.get_or_create(&ResultLabel { result });
        }
        // Both series exist from the start, at 0.
        for direction in ["inbound", "outbound"] {
            let _ = self.last_new.get_or_create(&DirectionLabels { direction });
        }
        let registry = metrics.registry_mut();
        registry.register_with_unit(
            "discovery_last_new_endpoint_timestamp",
            "Time an endpoint was last added to the template tree, 0 before",
            Unit::Seconds,
            self.last_new.clone(),
        );
        registry.register(
            "discovery_exchanges",
            "HTTP exchanges seen by discovery, by result",
            self.exchanges.clone(),
        );
        registry.register(
            "discovery_bodies",
            "Bodies of recorded exchanges, by result",
            self.bodies.clone(),
        );
        registry.register(
            "discovery_samples",
            "Sample admissions, by result",
            self.samples.clone(),
        );
        registry.register(
            "discovery_redactions",
            "Values redacted before storage, by location and rule",
            self.redactions.clone(),
        );
        registry.register(
            "discovery_endpoints",
            "Endpoints in the template tree",
            self.endpoints.clone(),
        );
        registry.register(
            "discovery_authorities",
            "Outbound scopes (authorities) held",
            self.authorities.clone(),
        );
        registry.register(
            "discovery_tunnels",
            "Outbound connections without plaintext HTTP, by kind and result",
            self.tunnels.clone(),
        );
        registry.register(
            "discovery_sample_bytes",
            "Bytes held by samples",
            self.sample_bytes.clone(),
        );
        registry.register(
            "discovery_pushes",
            "Settled or failed report push attempts, by result",
            self.pushes.clone(),
        );
        registry.register(
            "discovery_outbound_dropped",
            "Outbound parts dropped by the inbound-only fallback after an attempt with an unknown outcome",
            self.outbound_dropped.clone(),
        );
        registry.register_with_unit(
            "discovery_last_push_timestamp",
            "Time of the last acknowledged push, 0 before",
            Unit::Seconds,
            self.last_push.clone(),
        );
    }

    pub(super) fn exchange(&self, direction: &'static str, result: &'static str) {
        self.exchanges
            .get_or_create(&ExchangeLabels { direction, result })
            .inc();
    }

    pub(super) fn body(&self, direction: &'static str, part: &'static str, result: &'static str) {
        self.bodies
            .get_or_create(&BodyLabels {
                direction,
                part,
                result,
            })
            .inc();
    }

    pub(super) fn sample(&self, direction: &'static str, result: &'static str) {
        self.samples
            .get_or_create(&SampleLabels { direction, result })
            .inc();
    }

    pub(super) fn redaction(&self, location: &'static str, rule: &'static str, n: u64) {
        self.redactions
            .get_or_create(&RedactionLabels { location, rule })
            .inc_by(n);
    }

    /// A direction that never had an endpoint has no series: 0 does not create one.
    pub(super) fn set_endpoints(&self, direction: &'static str, n: usize) {
        let labels = DirectionLabels { direction };
        if n == 0 && self.endpoints.get(&labels).is_none() {
            return;
        }
        self.endpoints
            .get_or_create(&labels)
            .set(i64::try_from(n).unwrap_or(i64::MAX));
    }

    /// An exchange added an endpoint that this process had not recorded before.
    pub(super) fn new_endpoint(&self, direction: &'static str) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs());
        self.last_new
            .get_or_create(&DirectionLabels { direction })
            .set(i64::try_from(now).unwrap_or(i64::MAX));
    }

    pub(super) fn tunnel(&self, kind: &'static str, result: &'static str) {
        self.tunnels
            .get_or_create(&TunnelLabels { kind, result })
            .inc();
    }

    pub(super) fn set_authorities(&self, n: usize) {
        self.authorities.set(i64::try_from(n).unwrap_or(i64::MAX));
    }

    pub(super) fn set_sample_bytes(&self, n: usize) {
        self.sample_bytes.set(i64::try_from(n).unwrap_or(i64::MAX));
    }

    pub(super) fn push(&self, result: &'static str) {
        self.pushes.get_or_create(&ResultLabel { result }).inc();
    }

    pub(super) fn outbound_dropped(&self) {
        self.outbound_dropped.inc();
    }

    pub(super) fn set_last_push(&self, unix_seconds: u64) {
        self.last_push
            .set(i64::try_from(unix_seconds).unwrap_or(i64::MAX));
    }
}
