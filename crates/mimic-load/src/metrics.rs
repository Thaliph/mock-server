//! Generator metrics (spec §9): requests and their duration per plan endpoint.

use mimic_runtime::metrics::Metrics;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Unit;

/// As `mimic_http_request_duration_seconds`.
const DURATION_BUCKETS: [f64; 13] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
];

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct RequestLabels {
    endpoint: String,
    result: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct DurationLabels {
    endpoint: String,
}

type Durations = Family<DurationLabels, Histogram, fn() -> Histogram>;

fn new_histogram() -> Histogram {
    Histogram::new(DURATION_BUCKETS)
}

/// `endpoint` is `<METHOD> <template>` of a plan endpoint: the plan bounds the label set.
#[derive(Clone)]
pub struct LoadMetrics {
    requests: Family<RequestLabels, Counter>,
    durations: Durations,
    pub in_flight: Gauge,
    pub missed: Counter,
    pub targets: Gauge,
}

impl LoadMetrics {
    pub fn register(metrics: &mut Metrics) -> LoadMetrics {
        let load = LoadMetrics {
            requests: Family::default(),
            durations: Family::new_with_constructor(new_histogram as fn() -> Histogram),
            in_flight: Gauge::default(),
            missed: Counter::default(),
            targets: Gauge::default(),
        };
        let registry = metrics.registry_mut();
        registry.register(
            "load_requests",
            "Load requests by plan endpoint and result (status class or error kind)",
            load.requests.clone(),
        );
        registry.register_with_unit(
            "load_request_duration",
            "Time from the first byte written to the response head",
            Unit::Seconds,
            load.durations.clone(),
        );
        registry.register(
            "load_in_flight",
            "Load requests in flight",
            load.in_flight.clone(),
        );
        registry.register(
            "load_missed",
            "Sends due with the concurrency in flight",
            load.missed.clone(),
        );
        registry.register(
            "load_targets",
            "Targets of the last poll that are verified",
            load.targets.clone(),
        );
        load
    }

    pub fn result(&self, endpoint: &str, result: &'static str) {
        self.requests
            .get_or_create(&RequestLabels {
                endpoint: endpoint.to_string(),
                result,
            })
            .inc();
    }

    pub fn duration(&self, endpoint: &str, micros: u64) {
        self.durations
            .get_or_create(&DurationLabels {
                endpoint: endpoint.to_string(),
            })
            .observe(micros as f64 / 1_000_000.0);
    }
}
