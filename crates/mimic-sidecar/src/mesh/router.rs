//! The balancing state of the sidecar, kept across views (spec §7.9 "Load balancing"): per
//! Service port a [`Backend`] with its endpoints, their slow-start state and in-flight counts,
//! the pools, and the metrics. A view changes the endpoints; counts and connections of the
//! endpoints it keeps stay.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, RwLock};
use std::time::{Duration, Instant};

use mimic_core::mesh::lb::Endpoints;
use mimic_core::mesh::mirror::{self, Body, Candidate, Decision as Mirrored};
use mimic_core::mesh::outlier::{self, Outlier, Restoration};
use mimic_core::mesh::policy::{OutlierDetection, Policy};
use mimic_core::mesh::retry::{Budget, Decision, Next, Planner, Verdict, decide};
use mimic_core::mesh::route::{RouteView, Table};
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
    /// The failures and ejections of the endpoints, kept across views (spec §7.9 "Outlier
    /// detection"); one lock with the endpoints, so an outcome is recorded and a pick made
    /// under the same.
    outlier: Outlier,
    /// The policy of the view held.
    policy: OutlierDetection,
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

/// The retries of one Service (spec §7.9 "Retry budget"): its `retries` and the budget its
/// ports share. Carried over the views that hold the Service.
pub(crate) struct ServiceRetry {
    retries: AtomicU32,
    budget: Mutex<(u32, Budget)>,
}

impl ServiceRetry {
    fn new(policy: &Policy) -> ServiceRetry {
        let b = &policy.retry_budget;
        ServiceRetry {
            retries: AtomicU32::new(policy.retries),
            budget: Mutex::new((
                b.window_seconds,
                Budget::new(
                    b.percent,
                    b.min_per_second,
                    Duration::from_secs(u64::from(b.window_seconds)),
                ),
            )),
        }
    }

    /// A new view: `retries` and the rates apply at once; a new window starts the budget empty.
    fn configure(&self, policy: &Policy) {
        self.retries.store(policy.retries, Ordering::Release);
        let b = &policy.retry_budget;
        let mut held = lock(&self.budget);
        if held.0 == b.window_seconds {
            held.1.set_rates(b.percent, b.min_per_second);
        } else {
            *held = ServiceRetry::new(policy)
                .budget
                .into_inner()
                .unwrap_or_else(PoisonError::into_inner);
        }
    }

    pub(crate) fn retries(&self) -> u32 {
        self.retries.load(Ordering::Acquire)
    }

    /// A request: the first try of one, or an L4 connection.
    pub(crate) fn deposit(&self, now: Duration) {
        lock(&self.budget).1.deposit(now);
    }

    /// What to do with a failed try; the budget is asked only when the failure is retried and
    /// `planner` has a try left.
    pub(crate) fn decide(&self, verdict: Verdict, planner: &Planner, now: Duration) -> Decision {
        decide(verdict, planner, &mut lock(&self.budget).1, now)
    }

    fn counts(&self, now: Duration) -> (u64, u64) {
        lock(&self.budget).1.counts(now)
    }
}

/// At most one warning per second per backend, with the refusals not logged since the last.
#[derive(Default)]
struct Limiter {
    last: Option<Duration>,
    suppressed: u64,
}

impl Limiter {
    /// A refusal at `now`: `Some(suppressed)` when a line is due, `None` when it is counted
    /// for the next one.
    fn refused(&mut self, now: Duration) -> Option<u64> {
        let due = self
            .last
            .is_none_or(|last| now.saturating_sub(last) >= Duration::from_secs(1));
        if due {
            self.last = Some(now);
            Some(std::mem::take(&mut self.suppressed))
        } else {
            self.suppressed = self.suppressed.saturating_add(1);
            None
        }
    }
}

/// The routes of one Service port and the table that selects among their matches (spec §7.9
/// "Routing"), built when a view is applied.
pub(crate) struct Routed {
    pub routes: Vec<RouteView>,
    pub table: Table,
}

/// The endpoints of one Service port.
pub(crate) struct Backend {
    /// `<namespace>/<name>`.
    pub service: Arc<str>,
    /// `<namespace>/<name>:<port>`.
    pub label: Arc<str>,
    /// The retries and the budget of the Service.
    pub(crate) retry: Arc<ServiceRetry>,
    limiter: Mutex<Limiter>,
    state: Mutex<State>,
    /// The routes of the port in the view held; none when it has no route.
    routed: RwLock<Option<Arc<Routed>>>,
    /// The ClusterIP and port of the Service in the view held, where the fallback goes; none
    /// for a headless Service.
    original: Mutex<Option<SocketAddr>>,
}

impl Backend {
    fn new(
        service: &str,
        label: String,
        retry: Arc<ServiceRetry>,
        policy: &OutlierDetection,
    ) -> Backend {
        Backend {
            service: Arc::from(service),
            label: Arc::from(label),
            retry,
            limiter: Mutex::new(Limiter::default()),
            state: Mutex::new(State {
                set: Endpoints::new(Duration::ZERO),
                endpoints: Vec::new(),
                outlier: Outlier::new(policy),
                policy: policy.clone(),
            }),
            routed: RwLock::new(None),
            original: Mutex::new(None),
        }
    }

    /// The ClusterIP and port of the Service, none when it is headless.
    pub(crate) fn original(&self) -> Option<SocketAddr> {
        *lock(&self.original)
    }

    /// The routes of the port now, if it has any.
    pub(crate) fn routed(&self) -> Option<Arc<Routed>> {
        self.routed
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// A new view: the table of the routes that apply to `port`.
    fn set_routes(&self, routes: &[RouteView], port: u16) {
        let table = Table::new(routes, port);
        let routed = (!table.is_empty()).then(|| {
            Arc::new(Routed {
                routes: routes.to_vec(),
                table,
            })
        });
        *self.routed.write().unwrap_or_else(PoisonError::into_inner) = routed;
    }

    /// Replaces the endpoints and the outlier policy at `age`; returns the addresses that left.
    fn update(
        &self,
        addrs: &[SocketAddr],
        window: Duration,
        age: Duration,
        policy: &OutlierDetection,
        metrics: &MeshMetrics,
    ) -> Vec<SocketAddr> {
        let mut state = lock(&self.state);
        let restored = state.outlier.reconcile(policy, addrs, age);
        state.policy = policy.clone();
        let ejected = state.outlier.ejected_count(age);
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
        drop(state);
        self.restored(&restored, ejected, metrics);
        old.into_keys().collect()
    }

    /// The restorations of a view or a sweep: the gauge and one line each.
    fn restored(&self, restored: &[Restoration], ejected: usize, metrics: &MeshMetrics) {
        metrics.ejected_endpoints(&self.label, ejected);
        for r in restored {
            tracing::info!(
                service = &*self.service,
                backend = &*self.label,
                endpoint = %r.addr,
                cause = r.cause.label(),
                "endpoint restored"
            );
        }
    }

    /// The once-a-second sweep: ejections that ended are restored, the gauge follows.
    fn sweep(&self, router: &Router) {
        let now = router.started.elapsed();
        let (restored, ejected) = {
            let mut state = lock(&self.state);
            let addrs: Vec<SocketAddr> = state.endpoints.iter().map(|e| e.addr).collect();
            let policy = state.policy.clone();
            let restored = state.outlier.reconcile(&policy, &addrs, now);
            (restored, state.outlier.ejected_count(now))
        };
        self.restored(&restored, ejected, &router.metrics);
    }

    /// Records the outcome of one try to the pod `addr` (spec §7.9 "Outlier detection"): an
    /// ejection it causes is counted and logged.
    pub(crate) fn record(&self, router: &Router, addr: SocketAddr, outcome: outlier::Outcome) {
        let now = router.started.elapsed();
        let (ejection, ejected) = {
            let mut state = lock(&self.state);
            let ejection = state.outlier.record(addr, outcome, now);
            (ejection, state.outlier.ejected_count(now))
        };
        let Some(ejection) = ejection else {
            return;
        };
        router
            .metrics
            .ejection(&self.label, ejection.reason.label());
        router.metrics.ejected_endpoints(&self.label, ejected);
        tracing::info!(
            service = &*self.service,
            backend = &*self.label,
            endpoint = %addr,
            reason = ejection.reason.label(),
            ejection_seconds = ejection.seconds,
            ejections = ejection.ejections,
            "endpoint ejected"
        );
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
    /// Every endpoint is ejected: no pod, and no fallback for an L4 connection.
    AllEjected,
}

impl Backend {
    /// The budget refused a retry: counted, and logged at most once per second.
    pub(crate) fn exhausted(&self, router: &Router) {
        router.metrics.budget_exhausted(&self.label);
        let now = router.started.elapsed();
        let Some(suppressed) = lock(&self.limiter).refused(now) else {
            return;
        };
        let (window_requests, window_retries) = self.retry.counts(now);
        tracing::warn!(
            service = &*self.service,
            backend = &*self.label,
            window_requests,
            window_retries,
            suppressed,
            "retry budget exhausted"
        );
    }

    /// The next try of a request (spec §7.9 "Retries"): the planner chooses over the endpoints
    /// now, avoidance and slow start applying as in [`Backend::pick`].
    pub(crate) fn plan(&self, router: &Router, planner: &mut Planner) -> Plan {
        let now = router.started.elapsed();
        let avoid_now = now_ms();
        let state = lock(&self.state);
        let mut draw = |n: usize| router.draw(n);
        let outlier = &state.outlier;
        let next = planner.next_unejected(
            &state.set,
            now,
            |i| state.endpoints[i].load(avoid_now),
            &mut draw,
            |addr| outlier.is_ejected(addr, now),
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
            Next::Stop => {
                let all = !state.endpoints.is_empty()
                    && state
                        .endpoints
                        .iter()
                        .all(|e| state.outlier.is_ejected(e.addr, now));
                if all { Plan::AllEjected } else { Plan::Stop }
            }
        }
    }
}

/// What bounds the copies sent to mirrors (spec §7.9 "Mirrors").
pub(crate) struct MirrorConfig {
    pub max_in_flight: usize,
    pub max_in_flight_total: usize,
    pub timeout: Duration,
}

/// The copies in flight: in total and per mirror Service port (`<namespace>/<name>:<port>`).
struct Slots {
    max_port: usize,
    max_total: usize,
    held: Mutex<(usize, BTreeMap<String, usize>)>,
}

/// A slot taken by a copy, given back when dropped.
pub(crate) struct Slot {
    slots: Arc<Slots>,
    port: String,
}

impl Drop for Slot {
    fn drop(&mut self) {
        let mut held = lock(&self.slots.held);
        held.0 = held.0.saturating_sub(1);
        if let Some(n) = held.1.get_mut(&self.port) {
            *n = n.saturating_sub(1);
            if *n == 0 {
                held.1.remove(&self.port);
            }
        }
    }
}

pub(crate) struct Router {
    started: Instant,
    /// SplitMix64 state: one atomic add per draw, no lock.
    seed: AtomicU64,
    backends: RwLock<BTreeMap<String, Arc<Backend>>>,
    pub(crate) pools: Arc<Pools>,
    /// The pools of the copies sent to mirrors, apart from those of every other request.
    pub(crate) mirror_pools: Arc<Pools>,
    mirror_slots: Arc<Slots>,
    /// The deadline of a copy when the rule has no shorter timeout.
    pub(crate) mirror_timeout: Duration,
    pub(crate) metrics: MeshMetrics,
    /// `MIMIC_MESH_CONNECT_TIMEOUT`.
    pub(crate) connect_timeout: Duration,
    /// How long a pod whose connect failed is avoided.
    avoid: Duration,
    /// The retries and budget of each Service of the view held, by `<namespace>/<name>`.
    services: Mutex<BTreeMap<String, Arc<ServiceRetry>>>,
    /// The middle of the delay before a retry after a trusted refusal.
    pub(crate) refusal_delay: Duration,
    /// The route deadlines armed now (test hook, spec §7.9 "Timeouts").
    pub(crate) timers: Arc<AtomicUsize>,
}

impl Router {
    pub(crate) fn new(
        idle_timeout: Duration,
        max_labels: usize,
        seed: Option<u64>,
        connect_timeout: Duration,
        avoid: Duration,
        refusal_delay: Duration,
        mirrors: MirrorConfig,
    ) -> Router {
        Router {
            started: Instant::now(),
            seed: AtomicU64::new(seed.unwrap_or_else(|| rand::random_range(0..=u64::MAX))),
            backends: RwLock::new(BTreeMap::new()),
            pools: Pools::new(idle_timeout),
            mirror_pools: Pools::new(idle_timeout),
            mirror_slots: Arc::new(Slots {
                max_port: mirrors.max_in_flight,
                max_total: mirrors.max_in_flight_total,
                held: Mutex::new((0, BTreeMap::new())),
            }),
            mirror_timeout: mirrors.timeout,
            metrics: MeshMetrics::new(max_labels),
            connect_timeout,
            avoid,
            services: Mutex::new(BTreeMap::new()),
            refusal_delay,
            timers: Arc::default(),
        }
    }

    /// What each mirror of a rule does (spec §7.9 "Mirrors"): `candidates` are drawn or not,
    /// each drawn one that sends takes a slot, held until the guard is dropped.
    pub(crate) fn mirror_decide(
        &self,
        candidates: &[Candidate<'_>],
        body: Body,
        share: u64,
    ) -> Vec<Option<(Mirrored, Option<Slot>)>> {
        let slots = &self.mirror_slots;
        let mut held = lock(&slots.held);
        let decided = mirror::decide(
            candidates,
            slots.max_total.saturating_sub(held.0),
            |port| {
                slots
                    .max_port
                    .saturating_sub(held.1.get(port).copied().unwrap_or(0))
            },
            body,
            share,
        );
        decided
            .into_iter()
            .zip(candidates)
            .map(|(decision, candidate)| {
                let decision = decision?;
                let slot = decision.holds_slot().then(|| {
                    held.0 += 1;
                    *held.1.entry(candidate.port.to_string()).or_insert(0) += 1;
                    Slot {
                        slots: slots.clone(),
                        port: candidate.port.to_string(),
                    }
                });
                Some((decision, slot))
            })
            .collect()
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
        // The high bits of a 128-bit product: no modulo bias worth a branch.
        ((u128::from(self.random()) * n as u128) >> 64) as usize
    }

    /// A uniform 64-bit value (SplitMix64).
    pub(crate) fn random(&self) -> u64 {
        let mut z = self
            .seed
            .fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed)
            .wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// The sidecar's monotonic clock, the one the retry budget is given.
    pub(crate) fn now(&self) -> Duration {
        self.started.elapsed()
    }

    /// The once-a-second sweep of the outlier state of every backend.
    pub(crate) fn sweep(&self) {
        let all: Vec<Arc<Backend>> = self
            .backends
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect();
        for backend in all {
            backend.sweep(self);
        }
    }

    /// The budgets and the backends (and so the warning limiters) held.
    pub(crate) fn retry_state(&self) -> (usize, usize) {
        let backends = self.backends.read().unwrap_or_else(PoisonError::into_inner);
        (lock(&self.services).len(), backends.len())
    }

    /// The retries the budget of Service `key` (`<namespace>/<name>`) counts in its window.
    pub(crate) fn window_retries(&self, key: &str) -> u64 {
        let now = self.started.elapsed();
        lock(&self.services).get(key).map_or(0, |s| s.counts(now).1)
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
        let mut held = std::mem::take(&mut *lock(&self.services));
        for (key, service) in &view.services {
            let retry = held
                .remove(key)
                .unwrap_or_else(|| Arc::new(ServiceRetry::new(&service.policy)));
            retry.configure(&service.policy);
            lock(&self.services).insert(key.clone(), retry.clone());
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
                let backend = previous.remove(&label).unwrap_or_else(|| {
                    Arc::new(Backend::new(
                        key,
                        label.clone(),
                        retry.clone(),
                        &service.policy.outlier_detection,
                    ))
                });
                backend.set_routes(&service.routes, *port);
                *lock(&backend.original) = service
                    .ips
                    .first()
                    .map(|ip| SocketAddr::new(ip.to_canonical(), *port));
                self.metrics.backend_seen(&label);
                let outliers = &service.policy.outlier_detection;
                for gone in backend.update(&addrs, window, age, outliers, &self.metrics) {
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
        let names: BTreeSet<&str> = view
            .services
            .values()
            .flat_map(|service| service.routes.iter().map(|route| route.name.as_str()))
            .collect();
        self.metrics.routes_in_view(&names, now);
        self.pools.retain(&live);
        self.mirror_pools.retain(&live);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rty_51_the_limiter_logs_once_a_second_and_counts_what_it_held_back() {
        let mut limiter = Limiter::default();
        let at = |ms| Duration::from_millis(ms);
        assert_eq!(limiter.refused(at(0)), Some(0));
        assert_eq!(limiter.refused(at(10)), None);
        assert_eq!(limiter.refused(at(999)), None);
        assert_eq!(limiter.refused(at(1000)), Some(2));
        // A clock that goes back never logs early.
        assert_eq!(limiter.refused(at(500)), None);
        assert_eq!(limiter.refused(at(2000)), Some(1));
    }

    #[test]
    fn rty_28_every_picked_address_resolves_to_its_endpoint_by_index() {
        let retry = Arc::new(ServiceRetry::new(&mimic_core::mesh::policy::defaults()));
        let backend = Backend::new(
            "shop/cart",
            "shop/cart:80".to_string(),
            retry,
            &mimic_core::mesh::policy::defaults().outlier_detection,
        );
        let metrics = MeshMetrics::new(10_000);
        // Unsorted on purpose: the set sorts, and `endpoints` follows it.
        let addrs: Vec<SocketAddr> = (0..1000u16)
            .rev()
            .map(|n| SocketAddr::from(([10, 0, (n / 250) as u8, (n % 250) as u8], 8000 + n)))
            .collect();
        backend.update(
            &addrs,
            Duration::ZERO,
            Duration::ZERO,
            &mimic_core::mesh::policy::defaults().outlier_detection,
            &metrics,
        );
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
