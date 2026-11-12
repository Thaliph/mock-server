//! The mesh metrics of the sidecar (spec §7.9 "Load balancing"): requests, their durations and
//! the per-pod series, bounded by `MIMIC_METRICS_MAX_ENDPOINTS` and evicted 5 minutes after an
//! endpoint left the view.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::{Registry, Unit};

use crate::red::DURATION_BUCKETS;

/// The series of an endpoint that left the view stay this long.
pub(crate) const EVICT_AFTER: Duration = Duration::from_secs(5 * 60);

/// The label of the endpoints past `MIMIC_METRICS_MAX_ENDPOINTS`.
const OTHER: &str = "other";

/// Every `status_class` value, to remove the series of an evicted endpoint.
const CLASSES: [&str; 6] = ["1xx", "2xx", "3xx", "4xx", "5xx", "none"];

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct RequestLabels {
    service: String,
    route: &'static str,
    backend: String,
    status_class: &'static str,
    reason: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct DurationLabels {
    service: String,
    backend: String,
    status_class: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct EndpointLabels {
    backend: String,
    endpoint: String,
    status_class: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct InFlightLabels {
    backend: String,
    endpoint: String,
}

type Durations = Family<DurationLabels, Histogram, fn() -> Histogram>;

fn new_histogram() -> Histogram {
    Histogram::new(DURATION_BUCKETS)
}

struct Slot {
    label: Arc<str>,
    /// When the endpoint left the view; `None` while it is in it.
    left: Option<Instant>,
}

#[derive(Default)]
struct Slots {
    map: BTreeMap<(String, SocketAddr), Slot>,
    /// Slots with a label of their own.
    labelled: usize,
}

pub(crate) struct MeshMetrics {
    requests: Family<RequestLabels, Counter>,
    durations: Durations,
    endpoint_requests: Family<EndpointLabels, Counter>,
    in_flight: Family<InFlightLabels, Gauge>,
    slots: Mutex<Slots>,
    max_labels: usize,
}

impl MeshMetrics {
    pub(crate) fn new(max_labels: usize) -> MeshMetrics {
        MeshMetrics {
            requests: Family::default(),
            durations: Family::new_with_constructor(new_histogram as fn() -> Histogram),
            endpoint_requests: Family::default(),
            in_flight: Family::default(),
            slots: Mutex::new(Slots::default()),
            max_labels,
        }
    }

    pub(crate) fn register(&self, registry: &mut Registry) {
        registry.register(
            "mesh_requests",
            "Requests routed by the mesh, by Service, backend, status class and reason",
            self.requests.clone(),
        );
        registry.register_with_unit(
            "mesh_request_duration",
            "Time from the request to the response head of a mesh-routed request",
            Unit::Seconds,
            self.durations.clone(),
        );
        registry.register(
            "mesh_endpoint_requests",
            "Requests sent to each endpoint, by status class",
            self.endpoint_requests.clone(),
        );
        registry.register(
            "mesh_endpoint_in_flight",
            "Requests in flight to each endpoint",
            self.in_flight.clone(),
        );
    }

    /// The label of an endpoint, given when it first appears in a view: its address while a
    /// slot is free, else `other`. An endpoint back within the eviction time keeps its series.
    pub(crate) fn slot(&self, backend: &str, addr: SocketAddr) -> Arc<str> {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(slot) = slots.map.get_mut(&(backend.to_string(), addr)) {
            slot.left = None;
            return slot.label.clone();
        }
        let label: Arc<str> = if slots.labelled < self.max_labels {
            slots.labelled += 1;
            Arc::from(addr.to_string())
        } else {
            Arc::from(OTHER)
        };
        slots.map.insert(
            (backend.to_string(), addr),
            Slot {
                label: label.clone(),
                left: None,
            },
        );
        label
    }

    /// The endpoint left the view at `now`.
    pub(crate) fn left(&self, backend: &str, addr: SocketAddr, now: Instant) {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(slot) = slots.map.get_mut(&(backend.to_string(), addr)) {
            slot.left.get_or_insert(now);
        }
    }

    /// Removes the series of the endpoints that left the view 5 minutes before `now`.
    pub(crate) fn evict(&self, now: Instant) {
        let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
        let due: Vec<(String, SocketAddr)> = slots
            .map
            .iter()
            .filter(|(_, slot)| {
                slot.left
                    .is_some_and(|left| now.saturating_duration_since(left) >= EVICT_AFTER)
            })
            .map(|(key, _)| key.clone())
            .collect();
        for key in due {
            let Some(slot) = slots.map.remove(&key) else {
                continue;
            };
            if &*slot.label == OTHER {
                continue;
            }
            slots.labelled -= 1;
            let (backend, endpoint) = (key.0, slot.label.to_string());
            for status_class in CLASSES {
                self.endpoint_requests.remove(&EndpointLabels {
                    backend: backend.clone(),
                    endpoint: endpoint.clone(),
                    status_class,
                });
            }
            self.in_flight.remove(&InFlightLabels { backend, endpoint });
        }
    }

    pub(crate) fn in_flight_gauge(&self, backend: &str, endpoint: &str) -> Gauge {
        self.in_flight
            .get_or_create(&InFlightLabels {
                backend: backend.to_string(),
                endpoint: endpoint.to_string(),
            })
            .clone()
    }

    /// One mesh-routed request that got a response (or none: `status_class` `none`).
    pub(crate) fn request(&self, call: &Call<'_>, elapsed: Duration) {
        self.requests
            .get_or_create(&RequestLabels {
                service: call.service.to_string(),
                route: "none",
                backend: call.backend.to_string(),
                status_class: call.status_class,
                reason: call.reason,
            })
            .inc();
        self.durations
            .get_or_create(&DurationLabels {
                service: call.service.to_string(),
                backend: call.backend.to_string(),
                status_class: call.status_class,
            })
            .observe(elapsed.as_secs_f64());
        if let Some(endpoint) = call.endpoint {
            self.endpoint_requests
                .get_or_create(&EndpointLabels {
                    backend: call.backend.to_string(),
                    endpoint: endpoint.to_string(),
                    status_class: call.status_class,
                })
                .inc();
        }
    }
}

/// What one request is counted under.
pub(crate) struct Call<'a> {
    pub service: &'a str,
    pub backend: &'a str,
    pub endpoint: Option<&'a str>,
    pub status_class: &'static str,
    pub reason: &'static str,
}
