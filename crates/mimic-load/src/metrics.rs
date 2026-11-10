//! Generator metrics (spec §9): requests and their duration per plan endpoint.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use mimic_core::load::schedule::planned_rate;
use mimic_runtime::metrics::Metrics;
use prometheus_client::collector::Collector;
use prometheus_client::encoding::{DescriptorEncoder, EncodeLabelSet, EncodeMetric};
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::{ConstGauge, Gauge};
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

/// The open-model schedule of this pod, and when its run began (`None` while it waits for a
/// verified target).
#[derive(Debug, Clone, Copy)]
struct Planned {
    rate: u64,
    ramp_up_ms: u64,
    duration_ms: u64,
    begun: Option<Instant>,
}

/// `mimic_load_planned_rate`, evaluated from the monotonic clock at each scrape. Absent until
/// the parameters are known and in the closed model.
#[derive(Debug, Clone, Default)]
struct PlannedRate(Arc<Mutex<Option<Planned>>>);

impl PlannedRate {
    fn lock(&self) -> std::sync::MutexGuard<'_, Option<Planned>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Collector for PlannedRate {
    fn encode(&self, mut encoder: DescriptorEncoder) -> std::fmt::Result {
        let Some(planned) = *self.lock() else {
            return Ok(());
        };
        let rate = match planned.begun {
            Some(begun) => planned_rate(
                Some(planned.rate),
                planned.ramp_up_ms,
                planned.duration_ms,
                u64::try_from(begun.elapsed().as_millis()).unwrap_or(u64::MAX),
            )
            .unwrap_or(0.0),
            None => 0.0,
        };
        let gauge = ConstGauge::new(rate);
        let metric = encoder.encode_descriptor(
            "load_planned_rate",
            "Requests per second the schedule asks for now",
            None,
            gauge.metric_type(),
        )?;
        gauge.encode(metric)
    }
}

/// `endpoint` is `<METHOD> <template>` of a plan endpoint: the plan bounds the label set.
#[derive(Clone)]
pub struct LoadMetrics {
    requests: Family<RequestLabels, Counter>,
    durations: Durations,
    pub in_flight: Gauge,
    pub missed: Counter,
    pub targets: Gauge,
    planned: PlannedRate,
}

impl LoadMetrics {
    pub fn register(metrics: &mut Metrics) -> LoadMetrics {
        let load = LoadMetrics {
            requests: Family::default(),
            durations: Family::new_with_constructor(new_histogram as fn() -> Histogram),
            in_flight: Gauge::default(),
            missed: Counter::default(),
            targets: Gauge::default(),
            planned: PlannedRate::default(),
        };
        let registry = metrics.registry_mut();
        registry.register_collector(Box::new(load.planned.clone()));
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

    /// An open-model pod: `mimic_load_planned_rate` is 0 until [`LoadMetrics::begin`].
    pub fn plan_open(&self, rate: u64, ramp_up_ms: u64, duration_ms: u64) {
        *self.planned.lock() = Some(Planned {
            rate,
            ramp_up_ms,
            duration_ms,
            begun: None,
        });
    }

    /// The run's schedule starts at `at`.
    pub fn begin(&self, at: Instant) {
        if let Some(planned) = self.planned.lock().as_mut() {
            planned.begun = Some(at);
        }
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
