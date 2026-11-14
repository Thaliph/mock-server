//! The sidecar follows its mesh view (spec §7.9, design §4.3): a long-poll client of
//! `GET /v1/workloads/<deployment>/mesh`, the table of ClusterIP:port → Service it builds from
//! the view, the persisted last view, the classification of outbound connections, and the
//! balancing of the in-scope cleartext HTTP ones over the pods of the view (spec §7.9).

mod dial;
mod follow;
mod gate;
mod metrics;
mod pool;
pub mod replay;
mod router;
mod serve;
mod store;

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mimic_core::mesh::view::{Limits, View};
use mimic_runtime::config::{ConfigError, Env};
use mimic_runtime::metrics::Metrics;
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use prometheus_client::metrics::gauge::Gauge;
use prometheus_client::registry::Unit;

use crate::discovery::Target;
use router::Router;

pub use follow::Follower;
pub use replay::{Replay, ReplayBudget};
pub(crate) use router::Backend;
pub(crate) use serve::{Ended, Head, Serving, head_kind, serve};

const MESH_VAR: &str = "MIMIC_MESH";
const STALE_VAR: &str = "MIMIC_MESH_STALE_AFTER";
const STARTUP_VAR: &str = "MIMIC_MESH_STARTUP_WAIT";
const SERVICES_VAR: &str = "MIMIC_MESH_MAX_SERVICES";
const ENDPOINTS_VAR: &str = "MIMIC_MESH_MAX_ENDPOINTS";
const BYTES_VAR: &str = "MIMIC_MESH_MAX_VIEW_BYTES";
const POOL_IDLE_VAR: &str = "MIMIC_MESH_POOL_IDLE_TIMEOUT";
const CONNECT_VAR: &str = "MIMIC_MESH_CONNECT_TIMEOUT";
const OPAQUE_VAR: &str = "MIMIC_MESH_OPAQUE_PORTS";
const RETRY_BUFFER_VAR: &str = "MIMIC_MESH_RETRY_BUFFER";
const RETRY_TOTAL_VAR: &str = "MIMIC_MESH_RETRY_BUFFER_TOTAL";
const ENDPOINT_LABELS_VAR: &str = "MIMIC_METRICS_MAX_ENDPOINTS";

const KIB: usize = 1024;
const MIB: usize = 1024 * KIB;

/// A held request lasts as long as the controller's `MIMIC_MESH_WATCH_TIMEOUT` (at most 10 m).
const REQUEST_TIMEOUT: Duration = Duration::from_secs(11 * 60);

/// The ports of server-first or never-HTTP protocols (spec §7.9).
pub const DEFAULT_OPAQUE_PORTS: [u16; 10] = [25, 587, 3306, 4444, 5432, 6379, 9300, 11211, 22, 21];

/// Where the view comes from, and how it ages.
#[derive(Debug, Clone)]
pub struct MeshConfig {
    /// `MIMIC_MESH`: `off` (also unset) pulls no view and classifies nothing.
    pub enabled: bool,
    /// `MIMIC_MESH_STALE_AFTER`.
    pub stale_after: Duration,
    /// `MIMIC_MESH_STARTUP_WAIT`.
    pub startup_wait: Duration,
    /// `MIMIC_MESH_MAX_*`.
    pub limits: Limits,
    /// `<MIMIC_TRUST_DIR>/state/mesh.json`.
    pub state_file: PathBuf,
    /// `None` without `MIMIC_CONTROLLER_URL`: nothing is pulled.
    pub target: Option<Target>,
    /// Time allowed for one request, held ones included.
    pub timeout: Duration,
    /// `MIMIC_MESH_POOL_IDLE_TIMEOUT`.
    pub pool_idle_timeout: Duration,
    /// `MIMIC_MESH_CONNECT_TIMEOUT`: time allowed to connect to a pod.
    pub connect_timeout: Duration,
    /// How long a pod whose connect failed is avoided by the choice (spec §7.9); a field, not a
    /// variable: tests shorten it.
    pub avoid_failed_for: Duration,
    /// `MIMIC_METRICS_MAX_ENDPOINTS`.
    pub max_endpoint_labels: usize,
    /// `MIMIC_MESH_OPAQUE_PORTS`: in-scope connections to these ports keep the byte relay.
    pub opaque_ports: std::collections::BTreeSet<u16>,
    /// `MIMIC_MESH_RETRY_BUFFER`: request body bytes held per request so that it can be retried.
    pub retry_buffer: usize,
    /// `MIMIC_MESH_RETRY_BUFFER_TOTAL`: the same for all the requests of the sidecar together.
    pub retry_buffer_total: usize,
    /// Retries after the first try of a request (spec §7.9 "Retries"); a field, not a variable
    /// (`MimicConfig.retries` sets it from #446).
    pub retries: u32,
    /// The middle of the delay before a retry after a trusted refusal (±50 %); a field, not a
    /// variable: tests shorten it.
    pub refusal_delay: Duration,
    /// A fixed seed for the random draws (tests); `None`: seeded from the system.
    pub seed: Option<u64>,
}

impl Default for MeshConfig {
    fn default() -> MeshConfig {
        MeshConfig {
            enabled: false,
            stale_after: Duration::from_secs(60),
            startup_wait: Duration::from_secs(2),
            limits: Limits::default(),
            state_file: PathBuf::new(),
            target: None,
            timeout: REQUEST_TIMEOUT,
            pool_idle_timeout: Duration::from_secs(90),
            connect_timeout: Duration::from_secs(2),
            avoid_failed_for: Duration::from_secs(5),
            max_endpoint_labels: 256,
            opaque_ports: DEFAULT_OPAQUE_PORTS.into_iter().collect(),
            retry_buffer: 64 * KIB,
            retry_buffer_total: 8 * MIB,
            retries: 3,
            refusal_delay: Duration::from_millis(25),
            seed: None,
        }
    }
}

impl MeshConfig {
    pub fn from_env(env: &Env, trust_dir: &Path) -> Result<MeshConfig, ConfigError> {
        let defaults = MeshConfig::default();
        let enabled = match env.optional(MESH_VAR) {
            Some("on") => true,
            // Unset: as before M1. The webhook always writes it (spec §10.3).
            None | Some("off") => false,
            Some(other) => {
                return Err(ConfigError::invalid(MESH_VAR, other, "expected on or off"));
            }
        };
        let limits = Limits {
            max_services: number(env, SERVICES_VAR, defaults.limits.max_services, 100_000)?,
            max_endpoints: number(env, ENDPOINTS_VAR, defaults.limits.max_endpoints, 1_000_000)?,
            max_view_bytes: size(
                env,
                BYTES_VAR,
                defaults.limits.max_view_bytes,
                64 * KIB..=256 * MIB,
            )?,
        };
        Ok(MeshConfig {
            enabled,
            stale_after: duration(
                env,
                STALE_VAR,
                defaults.stale_after,
                Duration::from_secs(1),
                Duration::from_secs(3600),
            )?,
            startup_wait: duration(
                env,
                STARTUP_VAR,
                defaults.startup_wait,
                Duration::ZERO,
                Duration::from_secs(60),
            )?,
            limits,
            state_file: trust_dir.join("state").join("mesh.json"),
            target: Target::from_env(env)?,
            pool_idle_timeout: duration(
                env,
                POOL_IDLE_VAR,
                defaults.pool_idle_timeout,
                Duration::from_secs(1),
                Duration::from_secs(3600),
            )?,
            connect_timeout: duration(
                env,
                CONNECT_VAR,
                defaults.connect_timeout,
                Duration::from_millis(100),
                Duration::from_secs(10),
            )?,
            max_endpoint_labels: number(
                env,
                ENDPOINT_LABELS_VAR,
                defaults.max_endpoint_labels,
                10_000,
            )?,
            opaque_ports: opaque_ports(env)?,
            retry_buffer: size(env, RETRY_BUFFER_VAR, defaults.retry_buffer, 0..=MIB)?,
            retry_buffer_total: size(
                env,
                RETRY_TOTAL_VAR,
                defaults.retry_buffer_total,
                MIB..=64 * MIB,
            )?,
            ..defaults
        })
    }
}

/// `MIMIC_MESH_OPAQUE_PORTS`: at most 256 ports from 1 to 65535, or `none` (an empty value is
/// unset, like any variable).
fn opaque_ports(env: &Env) -> Result<std::collections::BTreeSet<u16>, ConfigError> {
    let Some(value) = env.optional(OPAQUE_VAR) else {
        return Ok(DEFAULT_OPAQUE_PORTS.into_iter().collect());
    };
    let invalid = |why: &str| ConfigError::invalid(OPAQUE_VAR, value, why.to_string());
    let mut ports = std::collections::BTreeSet::new();
    if value.trim() == "none" {
        return Ok(ports);
    }
    let entries: Vec<&str> = value.split(',').collect();
    if entries.len() > 256 {
        return Err(invalid("expected at most 256 ports"));
    }
    for entry in entries {
        match entry.trim().parse::<u16>() {
            Ok(port) if port > 0 => {
                ports.insert(port);
            }
            _ => return Err(invalid("expected comma-separated ports from 1 to 65535")),
        }
    }
    Ok(ports)
}

fn duration(
    env: &Env,
    var: &str,
    default: Duration,
    min: Duration,
    max: Duration,
) -> Result<Duration, ConfigError> {
    let value = env.duration_or(var, default)?;
    if (min..=max).contains(&value) {
        return Ok(value);
    }
    Err(ConfigError::invalid(
        var,
        env.optional(var).unwrap_or_default(),
        format!(
            "expected a duration from {}ms to {}ms",
            min.as_millis(),
            max.as_millis()
        ),
    ))
}

fn number(env: &Env, var: &str, default: usize, max: usize) -> Result<usize, ConfigError> {
    let value = env.parse_or(var, default)?;
    if (1..=max).contains(&value) {
        return Ok(value);
    }
    Err(ConfigError::invalid(
        var,
        &value.to_string(),
        format!("expected a number from 1 to {max}"),
    ))
}

/// `<n>`, `<n>Ki` or `<n>Mi` within `range`.
fn human(bytes: usize) -> String {
    match bytes {
        n if n >= MIB && n % MIB == 0 => format!("{}Mi", n / MIB),
        n if n >= KIB && n % KIB == 0 => format!("{}Ki", n / KIB),
        n => n.to_string(),
    }
}

fn size(
    env: &Env,
    var: &str,
    default: usize,
    range: std::ops::RangeInclusive<usize>,
) -> Result<usize, ConfigError> {
    let Some(value) = env.optional(var) else {
        return Ok(default);
    };
    let (digits, unit) = match value {
        v if v.ends_with("Ki") => (&v[..v.len() - 2], KIB),
        v if v.ends_with("Mi") => (&v[..v.len() - 2], MIB),
        v => (v, 1),
    };
    let bytes = (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()))
        .then(|| digits.parse::<usize>().ok())
        .flatten()
        .and_then(|n| n.checked_mul(unit));
    match bytes {
        Some(n) if range.contains(&n) => Ok(n),
        _ => Err(ConfigError::invalid(
            var,
            value,
            format!(
                "expected a size such as 4Mi (<n>, <n>Ki or <n>Mi) from {} to {}",
                human(*range.start()),
                human(*range.end())
            ),
        )),
    }
}

/// How long to wait after the `failures`-th failure in a row (0 for the first): 1, 2, 4 … 30 s,
/// times `factor` (0.5 to 1, a random number in the client), never below 1 s.
pub fn retry_delay(failures: u32, factor: f64) -> Duration {
    let base = 1u64 << failures.min(5);
    Duration::from_secs(base.min(30))
        .mul_f64(factor.clamp(0.5, 1.0))
        .max(Duration::from_secs(1))
}

/// What a lookup of an original destination says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// A ClusterIP and port of a Service of the view.
    InScope,
    /// Any other destination, also before the first view.
    Unscoped,
    /// The view of this workload says `"mesh": false`: nothing is classified.
    Off,
}

/// The table of a view: `<ClusterIP>:<port>` of each port of each Service. Built whole and
/// swapped whole, so a lookup reads one table or the other.
#[derive(Default)]
pub struct Table {
    /// A view was applied.
    loaded: bool,
    /// The view's `mesh` switch.
    enabled: bool,
    routes: HashMap<(IpAddr, u16), Arc<str>>,
    /// The balancing state of each route, when the table is built with a router.
    backends: HashMap<(IpAddr, u16), Arc<Backend>>,
}

impl Table {
    /// The table of `view` with the balancing state `router` holds for each of its routes.
    fn build_routed(view: &View, router: &Router) -> Table {
        let mut table = Table::build(view);
        for (key, service) in &view.services {
            for ip in &service.ips {
                for port in service.ports.keys() {
                    if let Some(backend) = router.backend(key, *port) {
                        table.backends.insert((ip.to_canonical(), *port), backend);
                    }
                }
            }
        }
        table
    }

    pub fn build(view: &View) -> Table {
        let mut routes = HashMap::new();
        for (key, service) in &view.services {
            let key: Arc<str> = Arc::from(key.as_str());
            for ip in &service.ips {
                for port in service.ports.keys() {
                    routes.insert((ip.to_canonical(), *port), key.clone());
                }
            }
        }
        Table {
            loaded: true,
            enabled: view.mesh,
            routes,
            backends: HashMap::new(),
        }
    }

    /// The Service whose ClusterIP and port these are.
    pub fn lookup(&self, ip: IpAddr, port: u16) -> Option<&str> {
        self.routes.get(&(ip.to_canonical(), port)).map(|k| &**k)
    }

    pub fn len(&self) -> usize {
        self.routes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct ResultLabel {
    result: &'static str,
}

/// The view held.
struct Held {
    view: Arc<View>,
    etag: String,
}

struct Contact {
    /// The last `200` or `304`, or the `fetchedAt` of a view read from the file.
    at: Option<Instant>,
    /// The stale warning of this streak is written.
    stale_warned: bool,
}

struct Inner {
    table: RwLock<Arc<Table>>,
    held: Mutex<Option<Held>>,
    contact: Mutex<Contact>,
    started: Instant,
    /// The sidecar pulls a view: readiness waits for it.
    follows: bool,
    stale_after: Duration,
    startup_wait: Duration,
    limits: Limits,
    state_file: PathBuf,
    replay: Arc<ReplayBudget>,
    retry_buffer: usize,
    pulls: Family<ResultLabel, Counter>,
    services: Gauge,
    endpoints: Gauge,
    age: Gauge,
    unscoped: Counter,
    router: Router,
    opaque_ports: std::collections::BTreeSet<u16>,
    /// Set once a view is held: readiness stays.
    seen: AtomicBool,
}

/// The view held by the sidecar, shared between the client, the listeners and the probes.
#[derive(Clone)]
pub struct MeshState(Arc<Inner>);

impl Default for MeshState {
    fn default() -> MeshState {
        MeshState::new(&MeshConfig::default())
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl MeshState {
    pub fn new(config: &MeshConfig) -> MeshState {
        MeshState(Arc::new(Inner {
            table: RwLock::new(Arc::default()),
            held: Mutex::new(None),
            contact: Mutex::new(Contact {
                at: None,
                stale_warned: false,
            }),
            started: Instant::now(),
            follows: config.enabled && config.target.is_some(),
            stale_after: config.stale_after,
            startup_wait: config.startup_wait,
            limits: config.limits.clone(),
            state_file: config.state_file.clone(),
            replay: ReplayBudget::new(config.retry_buffer_total, Gauge::default()),
            retry_buffer: config.retry_buffer,
            pulls: Family::default(),
            services: Gauge::default(),
            endpoints: Gauge::default(),
            age: Gauge::default(),
            unscoped: Counter::default(),
            opaque_ports: config.opaque_ports.clone(),
            router: Router::new(
                config.pool_idle_timeout,
                config.max_endpoint_labels,
                config.seed,
                config.connect_timeout,
                config.avoid_failed_for,
                config.retries,
                config.refusal_delay,
            ),
            seen: AtomicBool::new(false),
        }))
    }

    pub fn register(&self, metrics: &mut Metrics) {
        // Every series exists from the start, at 0.
        for result in ["ok", "not_modified", "older", "invalid", "error"] {
            let _ = self.0.pulls.get_or_create(&ResultLabel { result });
        }
        let registry = metrics.registry_mut();
        registry.register(
            "mesh_view_pulls",
            "Mesh view requests of the sidecar, by result",
            self.0.pulls.clone(),
        );
        registry.register_with_unit(
            "mesh_view_age",
            "Time since the last 200 or 304 of the mesh view, 0 without a view",
            Unit::Seconds,
            self.0.age.clone(),
        );
        registry.register(
            "mesh_view_services",
            "Services in the mesh view held by the sidecar",
            self.0.services.clone(),
        );
        registry.register(
            "mesh_view_endpoints",
            "Endpoints in the mesh view held by the sidecar",
            self.0.endpoints.clone(),
        );
        registry.register(
            "mesh_unscoped_connections",
            "Outbound connections whose destination is not a Service port of the mesh view",
            self.0.unscoped.clone(),
        );
        registry.register_with_unit(
            "mesh_retry_buffer",
            "Request body bytes held for replay by all the requests of the sidecar",
            Unit::Bytes,
            self.0.replay.gauge().clone(),
        );
        self.0.router.metrics.register(registry);
    }

    /// The buffer of a request that may be sent again (spec §7.9 "Retries", "Bodies").
    pub fn replay(&self) -> Replay {
        self.0.replay.replay(self.0.retry_buffer)
    }

    pub(crate) fn router(&self) -> &Router {
        &self.0.router
    }

    /// The balancing state of the Service port `dst` is the ClusterIP and port of, when it is
    /// in the view and the mesh is on for this workload.
    pub(crate) fn route(&self, dst: SocketAddr) -> Option<Arc<Backend>> {
        if self.0.opaque_ports.contains(&dst.port()) || self.stale() {
            return None;
        }
        let table = self.0.table.read().unwrap_or_else(PoisonError::into_inner);
        if !table.enabled {
            return None;
        }
        table
            .backends
            .get(&(dst.ip().to_canonical(), dst.port()))
            .cloned()
    }

    pub(crate) fn pull(&self, result: &'static str) {
        self.0.pulls.get_or_create(&ResultLabel { result }).inc();
    }

    /// The table in use now; a holder of it keeps this one whatever is swapped after.
    pub fn table(&self) -> Arc<Table> {
        self.0
            .table
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Classifies an original destination: one read of the table and one hash lookup.
    pub fn scope(&self, dst: SocketAddr) -> Scope {
        let table = self.0.table.read().unwrap_or_else(PoisonError::into_inner);
        if !table.loaded {
            Scope::Unscoped
        } else if !table.enabled {
            Scope::Off
        } else if table.lookup(dst.ip(), dst.port()).is_some() {
            Scope::InScope
        } else {
            Scope::Unscoped
        }
    }

    /// [`MeshState::scope`], counted when the destination is unscoped.
    pub fn classify(&self, dst: SocketAddr) -> Scope {
        let scope = self.scope(dst);
        if scope == Scope::Unscoped {
            self.0.unscoped.inc();
        }
        scope
    }

    pub fn unscoped_connections(&self) -> u64 {
        self.0.unscoped.get()
    }

    /// The `ETag` of the view held.
    pub fn etag(&self) -> Option<String> {
        lock(&self.0.held).as_ref().map(|h| h.etag.clone())
    }

    /// Holds `view` as pulled now, whatever it replaces.
    pub fn hold(&self, view: View, etag: &str) {
        self.install(Arc::new(view), etag, Instant::now(), "pull");
    }

    /// Whether `view` has a lower generation than the view held for an object both know. The keys
    /// name the object with its UID, so a recreated object is unknown, never older.
    fn regresses(&self, view: &View) -> bool {
        let held = lock(&self.0.held);
        let Some(held) = held.as_ref() else {
            return false;
        };
        let lower = |old: &std::collections::BTreeMap<String, i64>,
                     new: &std::collections::BTreeMap<String, i64>| {
            old.iter()
                .any(|(key, generation)| new.get(key).is_some_and(|n| n < generation))
        };
        lower(&held.view.generations, &view.generations)
            || held.view.services.iter().any(|(key, service)| {
                view.services
                    .get(key)
                    .is_some_and(|n| lower(&service.generations, &n.generations))
            })
    }

    /// Applies a pulled view unless it is older than the one held (design §4.3).
    pub(crate) fn offer(&self, view: Arc<View>, etag: &str) -> bool {
        if self.regresses(&view) {
            return false;
        }
        self.install(view, etag, Instant::now(), "pull");
        true
    }

    fn install(&self, view: Arc<View>, etag: &str, contact: Instant, source: &'static str) {
        self.0.router.apply(&view);
        let table = Arc::new(Table::build_routed(&view, &self.0.router));
        let (services, endpoints) = (view.services.len(), view.endpoint_count());
        *self.0.table.write().unwrap_or_else(PoisonError::into_inner) = table;
        self.0
            .services
            .set(i64::try_from(services).unwrap_or(i64::MAX));
        self.0
            .endpoints
            .set(i64::try_from(endpoints).unwrap_or(i64::MAX));
        let previous = lock(&self.0.held).replace(Held {
            view,
            etag: etag.to_string(),
        });
        drop(previous);
        self.0.seen.store(true, Ordering::Release);
        {
            let mut c = lock(&self.0.contact);
            c.at = Some(contact);
            c.stale_warned = false;
        }
        tracing::info!(
            target: "mesh",
            services,
            endpoints,
            etag,
            source,
            "mesh view applied"
        );
    }

    /// A `304`, or the same view again: the view held is fresh.
    pub(crate) fn touch(&self) {
        let mut c = lock(&self.0.contact);
        c.at = Some(Instant::now());
        c.stale_warned = false;
    }

    /// Test hook (#443): the next pooled checkout hands out a connection that is already
    /// closed, as a pod closing it between the readiness check and the write would.
    #[doc(hidden)]
    pub fn poison_next_checkout(&self) {
        self.0.router.pools.poison_next();
    }

    /// Test hook (#443): the next fresh connection to a pod (a reconnect) is refused.
    #[doc(hidden)]
    pub fn refuse_next_reconnect(&self) {
        self.0.router.pools.refuse_next_fresh();
    }

    /// The view says `"mesh": false`: the workload's routing is off (spec §7.9).
    pub(crate) fn off(&self) -> bool {
        let table = self.0.table.read().unwrap_or_else(PoisonError::into_inner);
        table.loaded && !table.enabled
    }

    /// The view is older than `MIMIC_MESH_STALE_AFTER`: its pod addresses are not used.
    pub(crate) fn stale(&self) -> bool {
        lock(&self.0.contact)
            .at
            .is_some_and(|at| at.elapsed() >= self.0.stale_after)
    }

    /// Removes the per-pod series of the endpoints that left the view 5 minutes before `now`.
    pub fn evict(&self, now: Instant) {
        self.0.router.metrics.evict(now);
    }

    /// Updates `mimic_mesh_view_age_seconds` and writes the stale warning once per streak.
    pub fn tick(&self) {
        self.evict(Instant::now());
        let (age, warn) = {
            let mut c = lock(&self.0.contact);
            let Some(at) = c.at else {
                return;
            };
            let age = at.elapsed();
            let warn = age >= self.0.stale_after && !std::mem::replace(&mut c.stale_warned, true);
            (age, warn)
        };
        self.0
            .age
            .set(i64::try_from(age.as_secs()).unwrap_or(i64::MAX));
        if warn {
            tracing::warn!(
                target: "mesh",
                age_seconds = age.as_secs(),
                stale_after_seconds = self.0.stale_after.as_secs(),
                "mesh view stale"
            );
        }
    }

    /// Ticks every second until `stop` completes.
    pub async fn run_age(self, stop: impl std::future::Future<Output = ()>) {
        let ticking = async {
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            loop {
                interval.tick().await;
                self.tick();
            }
        };
        tokio::select! {
            () = stop => {}
            () = ticking => {}
        }
    }

    /// `Ok` once the sidecar can take traffic: it holds a view, does not follow one, or has
    /// waited `MIMIC_MESH_STARTUP_WAIT` for it.
    pub fn ready(&self) -> Result<(), String> {
        if !self.0.follows
            || self.0.seen.load(Ordering::Acquire)
            || self.0.started.elapsed() >= self.0.startup_wait
        {
            Ok(())
        } else {
            Err("waiting for the mesh view".to_string())
        }
    }

    /// Reads the view of the state file, when there is a valid one (spec §7.9). Its mode is not
    /// applied: the mode comes from the mode state file.
    pub async fn restore(&self) {
        let path = self.0.state_file.clone();
        if path.as_os_str().is_empty() {
            return;
        }
        let limits = self.0.limits.clone();
        let read = tokio::task::spawn_blocking(move || store::read(&path, &limits))
            .await
            .unwrap_or_else(|_| Err("read task failed".to_string()));
        match read {
            Ok(Some((fetched_at, view, etag))) => {
                let age = Duration::from_secs(unix_now().saturating_sub(fetched_at));
                let contact = Instant::now().checked_sub(age).unwrap_or_else(Instant::now);
                self.install(Arc::new(view), &etag, contact, "state");
            }
            Ok(None) => {}
            Err(reason) => tracing::warn!(target: "mesh", reason, "mesh state ignored"),
        }
    }

    /// Writes the view held to the state file; a failure is a warning and nothing else.
    pub(crate) async fn save(&self, view: &View) {
        let path = self.0.state_file.clone();
        if path.as_os_str().is_empty() {
            return;
        }
        let json = view.to_json();
        let written = tokio::task::spawn_blocking(move || store::write(&path, unix_now(), &json))
            .await
            .unwrap_or_else(|_| Err("write task failed".to_string()));
        if let Err(reason) = written {
            tracing::warn!(target: "mesh", reason, "mesh state not saved");
        }
    }

    pub(crate) fn limits(&self) -> &Limits {
        &self.0.limits
    }
}
