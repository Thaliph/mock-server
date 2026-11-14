//! The mesh metrics of the sidecar (spec §7.9 "Load balancing"): requests, their durations and
//! the per-pod series, bounded by `MIMIC_METRICS_MAX_ENDPOINTS` and evicted 5 minutes after an
//! endpoint left the view.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::metrics::histogram::Histogram;
use prometheus_client::registry::{Registry, Unit};

use mimic_core::mesh::retry::Outcome;

use crate::red::DURATION_BUCKETS;

/// The series of an endpoint that left the view stay this long.
pub(crate) const EVICT_AFTER: Duration = Duration::from_secs(5 * 60);

/// The label of the endpoints past `MIMIC_METRICS_MAX_ENDPOINTS`.
const OTHER: &str = "other";

/// Every `result` of a try and `reason` of a retry or a skipped one, to remove the series of a
/// backend that left the view.
const RESULTS: [Outcome; 7] = [
    Outcome::Ok,
    Outcome::ConnectFailed,
    Outcome::RefusedStream,
    Outcome::Reset,
    Outcome::Overloaded,
    Outcome::QueueTimeout,
    Outcome::AppRefused,
];
const SKIPS: [&str; 3] = ["exhausted", "method", "buffer"];

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

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct BackendLabels {
    backend: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct ResultLabels {
    backend: String,
    result: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct ReasonLabels {
    backend: String,
    reason: &'static str,
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

/// What a backend (Service port) has made series of, to remove them when it leaves.
#[derive(Default)]
struct Series {
    /// (`status_class`, `reason`) of the `mimic_mesh_requests_total` series.
    requests: BTreeSet<(&'static str, &'static str)>,
    /// When the backend left the view; `None` while it is in it.
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
    reconnects: Family<BackendLabels, Counter>,
    fallbacks: Family<BackendLabels, Counter>,
    budget_exhausted: Family<BackendLabels, Counter>,
    attempts: Family<ResultLabels, Counter>,
    retries: Family<ReasonLabels, Counter>,
    skipped: Family<ReasonLabels, Counter>,
    /// `x-mimic-*` fields removed from app responses by the inbound side (spec §4.1).
    /// Without labels, and made by the first removal: nothing of the mesh shows until then
    /// (MSH-51).
    stripped: Family<Vec<(String, String)>, Counter>,
    /// Same as `stripped`: inbound connections closed on a response the codec cannot frame.
    unframed: Family<Vec<(String, String)>, Counter>,
    /// Same as `stripped`: inbound connections answered `503` `connect-failed`.
    inbound_refusals: Family<Vec<(String, String)>, Counter>,
    slots: Mutex<Slots>,
    /// By backend label.
    series: Mutex<BTreeMap<String, Series>>,
    max_labels: usize,
}

impl MeshMetrics {
    pub(crate) fn new(max_labels: usize) -> MeshMetrics {
        MeshMetrics {
            requests: Family::default(),
            durations: Family::new_with_constructor(new_histogram as fn() -> Histogram),
            endpoint_requests: Family::default(),
            in_flight: Family::default(),
            reconnects: Family::default(),
            fallbacks: Family::default(),
            budget_exhausted: Family::default(),
            attempts: Family::default(),
            retries: Family::default(),
            skipped: Family::default(),
            stripped: Family::default(),
            unframed: Family::default(),
            inbound_refusals: Family::default(),
            slots: Mutex::new(Slots::default()),
            series: Mutex::new(BTreeMap::new()),
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
        registry.register(
            "mesh_fallbacks",
            "Requests sent to the original destination instead of a pod",
            self.fallbacks.clone(),
        );
        registry.register(
            "mesh_retry_budget_exhausted",
            "Retries refused by the retry budget of the Service, by backend",
            self.budget_exhausted.clone(),
        );
        registry.register(
            "mesh_attempts",
            "Tries of mesh-routed requests, to a pod or to the original destination, by result",
            self.attempts.clone(),
        );
        registry.register(
            "mesh_retries",
            "Tries after the first of a mesh-routed request, by the reason of the retry",
            self.retries.clone(),
        );
        registry.register(
            "mesh_retries_skipped",
            "Failures of a retried kind that were not retried, by reason",
            self.skipped.clone(),
        );
        registry.register(
            "mesh_reconnects",
            "Requests sent again on a new connection because nothing of them was written",
            self.reconnects.clone(),
        );
        registry.register(
            "mesh_stripped_headers",
            "x-mimic-* fields removed from the responses of the app",
            self.stripped.clone(),
        );
        registry.register(
            "mesh_unframed_closes",
            "Inbound connections closed on a response of the app the codec cannot frame",
            self.unframed.clone(),
        );
        registry.register(
            "mesh_inbound_refusals",
            "Inbound connections answered 503 connect-failed because the app refused the connection",
            self.inbound_refusals.clone(),
        );
    }

    /// An inbound connection answered `503` `connect-failed` (spec §4.1).
    pub(crate) fn inbound_refusal(&self) {
        self.inbound_refusals.get_or_create(&Vec::new()).inc();
    }

    /// An inbound connection closed on a response the codec cannot frame.
    pub(crate) fn unframed_close(&self) {
        self.unframed.get_or_create(&Vec::new()).inc();
    }

    /// `n` fields removed from the response of the app.
    pub(crate) fn stripped(&self, n: usize) {
        self.stripped.get_or_create(&Vec::new()).inc_by(n as u64);
    }

    /// A request sent to the original destination instead of a pod.
    pub(crate) fn fallback(&self, backend: &str) {
        self.track(backend, None);
        self.fallbacks
            .get_or_create(&BackendLabels {
                backend: backend.to_string(),
            })
            .inc();
    }

    /// A retry the budget refused.
    pub(crate) fn budget_exhausted(&self, backend: &str) {
        self.track(backend, None);
        self.budget_exhausted
            .get_or_create(&BackendLabels {
                backend: backend.to_string(),
            })
            .inc();
    }

    /// One try of a request ended with `outcome`.
    pub(crate) fn attempt(&self, backend: &str, outcome: Outcome) {
        self.track(backend, None);
        self.attempts
            .get_or_create(&ResultLabels {
                backend: backend.to_string(),
                result: outcome.label(),
            })
            .inc();
    }

    /// A try after the first, because the one before ended with `outcome`.
    pub(crate) fn retry(&self, backend: &str, outcome: Outcome) {
        self.track(backend, None);
        self.retries
            .get_or_create(&ReasonLabels {
                backend: backend.to_string(),
                reason: outcome.label(),
            })
            .inc();
    }

    /// A failure of a retried kind that was not retried (`reason`: `Skip::label` or
    /// `exhausted`).
    pub(crate) fn skipped(&self, backend: &str, reason: &'static str) {
        self.track(backend, None);
        self.skipped
            .get_or_create(&ReasonLabels {
                backend: backend.to_string(),
                reason,
            })
            .inc();
    }

    /// The series of a backend exists from the moment it is in the view, at 0.
    pub(crate) fn backend_seen(&self, backend: &str) {
        let labels = BackendLabels {
            backend: backend.to_string(),
        };
        let _ = self.reconnects.get_or_create(&labels);
        let _ = self.fallbacks.get_or_create(&labels);
        let _ = self.budget_exhausted.get_or_create(&labels);
        self.series
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(backend.to_string())
            .or_default()
            .left = None;
    }

    /// Notes that a series of `backend` exists. One made after the backend left the view (a
    /// request that ends late) starts the 5 minutes anew.
    fn track(&self, backend: &str, request: Option<(&'static str, &'static str)>) {
        let mut all = self.series.lock().unwrap_or_else(PoisonError::into_inner);
        if !all.contains_key(backend) {
            all.insert(
                backend.to_string(),
                Series {
                    left: Some(Instant::now()),
                    ..Series::default()
                },
            );
        }
        if let (Some(series), Some(request)) = (all.get_mut(backend), request) {
            series.requests.insert(request);
        }
    }

    /// The backend left the view at `now`.
    pub(crate) fn backend_left(&self, backend: &str, now: Instant) {
        if let Some(series) = self
            .series
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get_mut(backend)
        {
            series.left.get_or_insert(now);
        }
    }

    /// A request that was sent again on a new connection.
    pub(crate) fn reconnect(&self, backend: &str) {
        self.track(backend, None);
        self.reconnects
            .get_or_create(&BackendLabels {
                backend: backend.to_string(),
            })
            .inc();
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
        self.evict_backends(now);
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
        // Backends that lost an endpoint labelled `other`: the series goes with the last one.
        let mut others = BTreeSet::new();
        for key in due {
            let Some(slot) = slots.map.remove(&key) else {
                continue;
            };
            if &*slot.label == OTHER {
                others.insert(key.0);
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
        for backend in others {
            let used = slots
                .map
                .iter()
                .any(|((b, _), slot)| *b == backend && &*slot.label == OTHER);
            if used {
                continue;
            }
            for status_class in CLASSES {
                self.endpoint_requests.remove(&EndpointLabels {
                    backend: backend.clone(),
                    endpoint: OTHER.to_string(),
                    status_class,
                });
            }
            self.in_flight.remove(&InFlightLabels {
                backend,
                endpoint: OTHER.to_string(),
            });
        }
    }

    /// Removes the per-Service series of the backends that left the view 5 minutes before `now`.
    fn evict_backends(&self, now: Instant) {
        let mut all = self.series.lock().unwrap_or_else(PoisonError::into_inner);
        all.retain(|backend, series| {
            let due = series
                .left
                .is_some_and(|left| now.saturating_duration_since(left) >= EVICT_AFTER);
            if !due {
                return true;
            }
            // `<namespace>/<name>:<port>`.
            let service = backend.rsplit_once(':').map_or(&**backend, |(s, _)| s);
            for &(status_class, reason) in &series.requests {
                self.requests.remove(&RequestLabels {
                    service: service.to_string(),
                    route: "none",
                    backend: backend.clone(),
                    status_class,
                    reason,
                });
            }
            for status_class in CLASSES {
                self.durations.remove(&DurationLabels {
                    service: service.to_string(),
                    backend: backend.clone(),
                    status_class,
                });
            }
            let labels = BackendLabels {
                backend: backend.clone(),
            };
            self.reconnects.remove(&labels);
            self.fallbacks.remove(&labels);
            self.budget_exhausted.remove(&labels);
            for outcome in RESULTS {
                self.attempts.remove(&ResultLabels {
                    backend: backend.clone(),
                    result: outcome.label(),
                });
                self.retries.remove(&ReasonLabels {
                    backend: backend.clone(),
                    reason: outcome.label(),
                });
            }
            for reason in SKIPS {
                self.skipped.remove(&ReasonLabels {
                    backend: backend.clone(),
                    reason,
                });
            }
            false
        });
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
        self.track(call.backend, Some((call.status_class, call.reason)));
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
    }

    /// One try written to the pod `endpoint`, with the `status_class` of its answer.
    pub(crate) fn endpoint_try(&self, backend: &str, endpoint: &str, status_class: &'static str) {
        self.endpoint_requests
            .get_or_create(&EndpointLabels {
                backend: backend.to_string(),
                endpoint: endpoint.to_string(),
                status_class,
            })
            .inc();
    }
}

/// What one request is counted under.
pub(crate) struct Call<'a> {
    pub service: &'a str,
    pub backend: &'a str,
    pub status_class: &'static str,
    pub reason: &'static str,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(metrics: &MeshMetrics) -> String {
        let mut registry = Registry::default();
        metrics.register(&mut registry);
        let mut out = String::new();
        prometheus_client::encoding::text::encode(&mut out, &registry).unwrap();
        out
    }

    fn addr(n: u8) -> SocketAddr {
        SocketAddr::from(([10, 0, 0, n], 80))
    }

    #[test]
    fn rty_29_the_other_series_goes_with_the_last_endpoint_that_had_the_label() {
        let metrics = MeshMetrics::new(1);
        let backend = "shop/cart:80";
        assert_eq!(&*metrics.slot(backend, addr(1)), "10.0.0.1:80");
        let (b, c) = (
            metrics.slot(backend, addr(2)),
            metrics.slot(backend, addr(3)),
        );
        assert_eq!((&*b, &*c), ("other", "other"));
        metrics.endpoint_try(backend, "other", "2xx");
        let _ = metrics.in_flight_gauge(backend, "other");
        assert!(text(&metrics).contains("endpoint=\"other\""));
        let now = Instant::now();
        let after = now + EVICT_AFTER;
        // One of the two `other` endpoints left: the label is still in use.
        metrics.left(backend, addr(2), now);
        metrics.evict(after);
        assert!(text(&metrics).contains("endpoint=\"other\""));
        // The last one left.
        metrics.left(backend, addr(3), now);
        metrics.evict(after);
        assert!(
            !text(&metrics).contains("endpoint=\"other\""),
            "{}",
            text(&metrics)
        );
    }

    #[test]
    fn rty_29_a_backend_that_left_keeps_no_endpoint_series() {
        let metrics = MeshMetrics::new(10);
        let backend = "shop/cart:80";
        let label = metrics.slot(backend, addr(1));
        metrics.endpoint_try(backend, &label, "2xx");
        let _ = metrics.in_flight_gauge(backend, &label);
        metrics.backend_seen(backend);
        let now = Instant::now();
        metrics.backend_left(backend, now);
        metrics.left(backend, addr(1), now);
        metrics.evict(now + EVICT_AFTER);
        assert!(
            !text(&metrics).contains("10.0.0.1:80"),
            "{}",
            text(&metrics)
        );
    }
}
