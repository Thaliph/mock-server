//! RED metrics of the proxy (spec §9): requests and their duration per authority, connections
//! and bytes. The `authority` label is bounded per direction and metric by an
//! [`AuthorityBudget`]; the rest is counted under `(other)`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use mimic_core::proxy::authority::{AuthorityBudget, status_class};
use mimic_runtime::metrics::Metrics;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::Unit;

use crate::listener::Direction;

/// Authorities kept per direction and metric unless configured otherwise.
pub const DEFAULT_MAX_AUTHORITIES: usize = 50;

const DURATION_BUCKETS: [f64; 13] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0,
];

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct RequestLabels {
    direction: &'static str,
    protocol: &'static str,
    authority: String,
    status_class: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct DurationLabels {
    direction: &'static str,
    authority: String,
    status_class: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct ConnectionLabels {
    direction: &'static str,
    protocol: &'static str,
    authority: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct BytesLabels {
    direction: &'static str,
    protocol: &'static str,
    flow: &'static str,
}

type Durations = Family<DurationLabels, Histogram, fn() -> Histogram>;

fn new_histogram() -> Histogram {
    Histogram::new(DURATION_BUCKETS)
}

/// The proxy's request, connection and byte counters.
#[derive(Clone)]
pub struct ProxyMetrics {
    requests: Family<RequestLabels, Counter>,
    durations: Durations,
    connections: Family<ConnectionLabels, Counter>,
    bytes: Family<BytesLabels, Counter>,
    /// One budget per (metric, direction); the set restarts empty with the process.
    budgets: Arc<Mutex<BTreeMap<(&'static str, &'static str), AuthorityBudget>>>,
    max_authorities: usize,
}

impl Default for ProxyMetrics {
    fn default() -> ProxyMetrics {
        ProxyMetrics::new(DEFAULT_MAX_AUTHORITIES)
    }
}

impl ProxyMetrics {
    /// Metrics that are not exposed anywhere.
    pub fn new(max_authorities: usize) -> ProxyMetrics {
        ProxyMetrics {
            requests: Family::default(),
            durations: Family::new_with_constructor(new_histogram as fn() -> Histogram),
            connections: Family::default(),
            bytes: Family::default(),
            budgets: Arc::default(),
            max_authorities,
        }
    }

    pub fn register(metrics: &mut Metrics, max_authorities: usize) -> ProxyMetrics {
        let proxy = ProxyMetrics::new(max_authorities);
        let registry = metrics.registry_mut();
        registry.register(
            "http_requests",
            "HTTP requests relayed, by status class of the response",
            proxy.requests.clone(),
        );
        registry.register_with_unit(
            "http_request_duration",
            "Time from the request to the response head",
            Unit::Seconds,
            proxy.durations.clone(),
        );
        registry.register(
            "proxy_connections",
            "Connections handled by the proxy, by detected protocol",
            proxy.connections.clone(),
        );
        registry.register(
            "proxy_bytes",
            "Bytes relayed (body bytes for HTTP)",
            proxy.bytes.clone(),
        );
        proxy
    }

    /// The label to use for `authority` in `metric`, within the budget.
    fn label(&self, metric: &'static str, direction: Direction, authority: &str) -> String {
        let mut budgets = self
            .budgets
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        budgets
            .entry((metric, direction.as_str()))
            .or_insert_with(|| AuthorityBudget::new(self.max_authorities))
            .label(authority)
    }

    /// One HTTP request answered (or failed, `status` `None`) after `head` time.
    pub(crate) fn request(
        &self,
        direction: Direction,
        protocol: &'static str,
        authority: &str,
        status: Option<u16>,
        head: Duration,
    ) {
        let class = status.map_or("none", status_class);
        self.requests
            .get_or_create(&RequestLabels {
                direction: direction.as_str(),
                protocol,
                authority: self.label("requests", direction, authority),
                status_class: class,
            })
            .inc();
        self.durations
            .get_or_create(&DurationLabels {
                direction: direction.as_str(),
                authority: self.label("duration", direction, authority),
                status_class: class,
            })
            .observe(head.as_secs_f64());
    }

    pub(crate) fn connection(&self, direction: Direction, protocol: &'static str, authority: &str) {
        self.connections
            .get_or_create(&ConnectionLabels {
                direction: direction.as_str(),
                protocol,
                authority: self.label("connections", direction, authority),
            })
            .inc();
    }

    pub(crate) fn bytes(
        &self,
        direction: Direction,
        protocol: &'static str,
        to_upstream: u64,
        to_client: u64,
    ) {
        for (flow, n) in [("to_upstream", to_upstream), ("to_client", to_client)] {
            self.bytes
                .get_or_create(&BytesLabels {
                    direction: direction.as_str(),
                    protocol,
                    flow,
                })
                .inc_by(n);
        }
    }
}
