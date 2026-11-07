//! `mimic_egress_*` metrics of the pull client (spec §9).

use mimic_runtime::metrics::Metrics;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Unit;

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct ResultLabel {
    result: &'static str,
}

/// Unregistered until [`EgressMetrics::register`].
#[derive(Clone, Default)]
pub struct EgressMetrics {
    pulls: Family<ResultLabel, Counter>,
    endpoints: Gauge,
    bytes: Gauge,
    last_pull: Gauge,
}

impl EgressMetrics {
    pub fn register(&self, metrics: &mut Metrics) {
        // Every series exists from the start, at 0.
        for result in [
            "ok",
            "not_modified",
            "too_large",
            "invalid",
            "unsupported",
            "error",
        ] {
            let _ = self.pulls.get_or_create(&ResultLabel { result });
        }
        let registry = metrics.registry_mut();
        registry.register(
            "egress_pulls",
            "Egress pull attempts of the sidecar, by result",
            self.pulls.clone(),
        );
        registry.register(
            "egress_endpoints",
            "Endpoints in the egress snapshot held by the sidecar",
            self.endpoints.clone(),
        );
        registry.register_with_unit(
            "egress",
            "Size of the egress snapshot's body as received by the sidecar, 0 without one",
            Unit::Bytes,
            self.bytes.clone(),
        );
        registry.register_with_unit(
            "egress_last_pull_timestamp",
            "Time of the last egress pull of the sidecar answered 200 or 304, 0 before",
            Unit::Seconds,
            self.last_pull.clone(),
        );
    }

    pub(super) fn pull(&self, result: &'static str) {
        self.pulls.get_or_create(&ResultLabel { result }).inc();
    }

    pub(super) fn set_snapshot(&self, endpoints: usize, bytes: usize) {
        self.endpoints
            .set(i64::try_from(endpoints).unwrap_or(i64::MAX));
        self.bytes.set(i64::try_from(bytes).unwrap_or(i64::MAX));
    }

    pub(super) fn set_last_pull(&self, unix_seconds: u64) {
        self.last_pull
            .set(i64::try_from(unix_seconds).unwrap_or(i64::MAX));
    }
}
