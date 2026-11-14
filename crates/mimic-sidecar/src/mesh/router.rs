//! The balancing state of the sidecar, kept across views (spec §7.9 "Load balancing"): per
//! Service port a [`Backend`] with its endpoints, their slow-start state and in-flight counts,
//! the pools, and the metrics. A view changes the endpoints; counts and connections of the
//! endpoints it keeps stay.

use std::collections::{BTreeMap, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, RwLock};
use std::time::{Duration, Instant};

use mimic_core::mesh::lb::Endpoints;
use mimic_core::mesh::retry::{Next, Planner};
use mimic_core::mesh::view::View;
use prometheus_client::metrics::gauge::Gauge;

use super::metrics::MeshMetrics;
use super::pool::Pools;

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Milliseconds since the first call, plus one (0 is "never").
fn now_ms() -> u64 {
    static BASE: OnceLock<Instant> = OnceLock::new();
    u64::try_from(BASE.get_or_init(Instant::now).elapsed().as_millis())
        .unwrap_or(u64::MAX - 1)
        .saturating_add(1)
}

/// What the choice adds to the in-flight count of a pod whose connect failed (spec §7.9): it is
/// picked only when no other endpoint can be.
const AVOIDED: u32 = 1 << 20;

/// One pod of a backend.
pub(crate) struct Endpoint {
    pub addr: SocketAddr,
    /// The `endpoint` metric label: `ip:port` or `other`.
    pub label: Arc<str>,
    in_flight: AtomicU32,
    /// Until when the choice avoids this pod (`now_ms`), 0 when it does not.
    failed_until: AtomicU64,
    gauge: Gauge,
}

impl Endpoint {
    /// The count the choice compares: requests in flight, and a penalty while avoided.
    fn load(&self, now: u64) -> u32 {
        let in_flight = self.in_flight.load(Ordering::Acquire);
        if self.failed_until.load(Ordering::Acquire) > now {
            in_flight.saturating_add(AVOIDED)
        } else {
            in_flight
        }
    }
}

/// A request in flight to an endpoint, counted until dropped.
pub(crate) struct InFlight(Arc<Endpoint>);

impl InFlight {
    pub(crate) fn endpoint(&self) -> &Endpoint {
        &self.0
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::AcqRel);
        self.0.gauge.dec();
    }
}

struct State {
    set: Endpoints,
    /// Parallel to the indices of `set`.
    endpoints: Vec<Arc<Endpoint>>,
}

impl State {
    /// The endpoint of an address `set` picked: `endpoints` follows the sorted order of the
    /// set, so a binary search, never a scan under the lock.
    fn endpoint(&self, addr: SocketAddr) -> Option<&Arc<Endpoint>> {
        let i = self
            .endpoints
            .binary_search_by_key(&addr, |e| e.addr)
            .ok()?;
        self.endpoints.get(i)
    }
}

/// The endpoints of one Service port.
pub(crate) struct Backend {
    /// `<namespace>/<name>`.
    pub service: Arc<str>,
    /// `<namespace>/<name>:<port>`.
    pub label: Arc<str>,
    state: Mutex<State>,
}

impl Backend {
    fn new(service: &str, label: String) -> Backend {
        Backend {
            service: Arc::from(service),
            label: Arc::from(label),
            state: Mutex::new(State {
                set: Endpoints::new(Duration::ZERO),
                endpoints: Vec::new(),
            }),
        }
    }

    /// Replaces the endpoints; returns the addresses that left.
    fn update(
        &self,
        addrs: &[SocketAddr],
        window: Duration,
        age: Duration,
        metrics: &MeshMetrics,
    ) -> Vec<SocketAddr> {
        let mut state = lock(&self.state);
        state.set.set_window(window);
        state.set.update(addrs, age);
        let mut old: BTreeMap<SocketAddr, Arc<Endpoint>> =
            state.endpoints.drain(..).map(|e| (e.addr, e)).collect();
        let mut endpoints = Vec::with_capacity(state.set.len());
        for i in 0..state.set.len() {
            let Some(addr) = state.set.addr(i) else {
                continue;
            };
            let endpoint = old.remove(&addr).unwrap_or_else(|| {
                let label = metrics.slot(&self.label, addr);
                let gauge = metrics.in_flight_gauge(&self.label, &label);
                Arc::new(Endpoint {
                    addr,
                    label,
                    in_flight: AtomicU32::new(0),
                    failed_until: AtomicU64::new(0),
                    gauge,
                })
            });
            endpoints.push(endpoint);
        }
        state.endpoints = endpoints;
        old.into_keys().collect()
    }

    /// The addresses of the endpoints now.
    fn addrs(&self) -> Vec<SocketAddr> {
        lock(&self.state).endpoints.iter().map(|e| e.addr).collect()
    }
}

/// Where the next try of a request goes.
pub(crate) enum Plan {
    /// A pod, counted in flight.
    Pod(InFlight),
    /// The original destination (the ClusterIP).
    Original,
    /// No try is left, or the backend has no endpoint.
    Stop,
}

impl Backend {
    /// The next try of a request (spec §7.9 "Retries"): the planner chooses over the endpoints
    /// now, avoidance and slow start applying as in [`Backend::pick`].
    pub(crate) fn plan(&self, router: &Router, planner: &mut Planner) -> Plan {
        let now = router.started.elapsed();
        let avoid_now = now_ms();
        let state = lock(&self.state);
        let mut draw = |n: usize| router.draw(n);
        let next = planner.next(
            &state.set,
            now,
            |i| state.endpoints[i].load(avoid_now),
            &mut draw,
        );
        match next {
            Next::Pod(addr) => {
                let Some(endpoint) = state.endpoint(addr).cloned() else {
                    return Plan::Stop;
                };
                drop(state);
                endpoint.in_flight.fetch_add(1, Ordering::AcqRel);
                endpoint.gauge.inc();
                Plan::Pod(InFlight(endpoint))
            }
            Next::Original => Plan::Original,
            Next::Stop => Plan::Stop,
        }
    }
}

pub(crate) struct Router {
    started: Instant,
    /// SplitMix64 state: one atomic add per draw, no lock.
    seed: AtomicU64,
    backends: RwLock<BTreeMap<String, Arc<Backend>>>,
    pub(crate) pools: Arc<Pools>,
    pub(crate) metrics: MeshMetrics,
    /// `MIMIC_MESH_CONNECT_TIMEOUT`.
    pub(crate) connect_timeout: Duration,
    /// How long a pod whose connect failed is avoided.
    avoid: Duration,
    /// Retries after the first try of a request.
    pub(crate) retries: u32,
    /// The middle of the delay before a retry after a trusted refusal.
    pub(crate) refusal_delay: Duration,
}

impl Router {
    pub(crate) fn new(
        idle_timeout: Duration,
        max_labels: usize,
        seed: Option<u64>,
        connect_timeout: Duration,
        avoid: Duration,
        retries: u32,
        refusal_delay: Duration,
    ) -> Router {
        Router {
            started: Instant::now(),
            seed: AtomicU64::new(seed.unwrap_or_else(|| rand::random_range(0..=u64::MAX))),
            backends: RwLock::new(BTreeMap::new()),
            pools: Pools::new(idle_timeout),
            metrics: MeshMetrics::new(max_labels),
            connect_timeout,
            avoid,
            retries,
            refusal_delay,
        }
    }

    /// Connecting to the pod failed: the choice avoids it for a while.
    pub(crate) fn failed(&self, pick: &InFlight) {
        let until = now_ms().saturating_add(u64::try_from(self.avoid.as_millis()).unwrap_or(0));
        pick.endpoint().failed_until.store(until, Ordering::Release);
    }

    /// A connection to the pod was had: it is not avoided any more.
    pub(crate) fn succeeded(&self, pick: &InFlight) {
        pick.endpoint().failed_until.store(0, Ordering::Release);
    }

    /// A number in `[0, 1]`.
    pub(crate) fn unit(&self) -> f64 {
        const STEPS: usize = 1_000_000;
        self.draw(STEPS + 1) as f64 / STEPS as f64
    }

    /// A number below `n`.
    fn draw(&self, n: usize) -> usize {
        let mut z = self
            .seed
            .fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed)
            .wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^= z >> 31;
        // The high bits of a 128-bit product: no modulo bias worth a branch.
        ((u128::from(z) * n as u128) >> 64) as usize
    }

    /// The backend of a Service port.
    pub(crate) fn backend(&self, service: &str, port: u16) -> Option<Arc<Backend>> {
        self.backends
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&format!("{service}:{port}"))
            .cloned()
    }

    /// Applies a view: the endpoints of every Service port, the pools of the endpoints that
    /// are gone drained, the series of the ones that left marked for eviction.
    pub(crate) fn apply(&self, view: &View) {
        let now = Instant::now();
        let age = self.started.elapsed();
        let mut backends = self
            .backends
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let mut previous = std::mem::take(&mut *backends);
        let mut live = HashSet::new();
        for (key, service) in &view.services {
            let window =
                Duration::from_secs(u64::from(service.policy.load_balancer.slow_start_seconds));
            for port in service.ports.keys() {
                let label = format!("{key}:{port}");
                let addrs: Vec<SocketAddr> = service
                    .endpoints
                    .get(port)
                    .map(|list| {
                        list.iter()
                            .map(|e| SocketAddr::new(e.ip.to_canonical(), e.port))
                            .collect()
                    })
                    .unwrap_or_default();
                let backend = previous
                    .remove(&label)
                    .unwrap_or_else(|| Arc::new(Backend::new(key, label.clone())));
                self.metrics.backend_seen(&label);
                for gone in backend.update(&addrs, window, age, &self.metrics) {
                    self.metrics.left(&backend.label, gone, now);
                }
                live.extend(backend.addrs());
                // The pools of the original destination (the fallback) stay.
                live.extend(
                    service
                        .ips
                        .iter()
                        .map(|ip| SocketAddr::new(ip.to_canonical(), *port)),
                );
                backends.insert(label, backend);
            }
        }
        // A Service port that left the view: every endpoint of it left.
        for (_, backend) in previous {
            self.metrics.backend_left(&backend.label, now);
            for addr in backend.addrs() {
                self.metrics.left(&backend.label, addr, now);
            }
        }
        drop(backends);
        self.pools.retain(&live);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rty_28_every_picked_address_resolves_to_its_endpoint_by_index() {
        let backend = Backend::new("shop/cart", "shop/cart:80".to_string());
        let metrics = MeshMetrics::new(10_000);
        // Unsorted on purpose: the set sorts, and `endpoints` follows it.
        let addrs: Vec<SocketAddr> = (0..1000u16)
            .rev()
            .map(|n| SocketAddr::from(([10, 0, (n / 250) as u8, (n % 250) as u8], 8000 + n)))
            .collect();
        backend.update(&addrs, Duration::ZERO, Duration::ZERO, &metrics);
        let state = lock(&backend.state);
        for addr in &addrs {
            assert_eq!(state.endpoint(*addr).map(|e| e.addr), Some(*addr));
        }
        assert!(
            state
                .endpoint(SocketAddr::from(([10, 9, 9, 9], 1)))
                .is_none()
        );
        for (i, endpoint) in state.endpoints.iter().enumerate() {
            assert_eq!(
                state.set.addr(i),
                Some(endpoint.addr),
                "parallel to the set"
            );
        }
    }
}
