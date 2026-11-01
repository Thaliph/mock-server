//! `mimic_discovery_*` metrics (spec §9).

use mimic_runtime::metrics::Metrics;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;

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

/// Shared by the taps (which count what they drop) and the recorder. Unregistered until
/// [`DiscoveryMetrics::register`].
#[derive(Clone, Default)]
pub struct DiscoveryMetrics {
    exchanges: Family<ExchangeLabels, Counter>,
    bodies: Family<BodyLabels, Counter>,
    samples: Family<SampleLabels, Counter>,
    redactions: Family<RedactionLabels, Counter>,
    endpoints: Family<DirectionLabels, Gauge>,
    sample_bytes: Gauge,
}

impl DiscoveryMetrics {
    pub fn register(&self, metrics: &mut Metrics) {
        let registry = metrics.registry_mut();
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
            "discovery_sample_bytes",
            "Bytes held by samples",
            self.sample_bytes.clone(),
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

    pub(super) fn set_endpoints(&self, direction: &'static str, n: usize) {
        self.endpoints
            .get_or_create(&DirectionLabels { direction })
            .set(i64::try_from(n).unwrap_or(i64::MAX));
    }

    pub(super) fn set_sample_bytes(&self, n: usize) {
        self.sample_bytes.set(i64::try_from(n).unwrap_or(i64::MAX));
    }
}
