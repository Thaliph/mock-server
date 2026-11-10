//! The generator (spec §7.8 "Generator"): reads the plan, verifies the targets, sends the
//! load and reports. All the choices (which request, how many sends, which results) are the
//! pure functions of `mimic_core::load`; this module owns the sockets, the timers and the
//! random numbers it hands to them.

use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use mimic_core::load::plan::Plan;
use mimic_core::load::request::{Context, Error as RequestError, request};
use mimic_core::load::schedule::{Pacer, workers};
use mimic_core::load::summary::{ErrorKind, PodResults, Stats};
use mimic_runtime::admin::Shutdown;
use tokio::sync::watch;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::sleep;

use crate::api::{Api, ApiError, PodParams, Post};
use crate::config::Config;
use crate::http::{Conn, Marker, Outcome, Prepared, probe};
use crate::metrics::LoadMetrics;

/// Intervals without an answer of the load API after which the generator stops.
const CONTROL_INTERVALS: u32 = 3;
/// Time the workers get to wind down once a stop is decided.
const WIND_DOWN: Duration = Duration::from_secs(2);

/// Why the generator stopped. `Completed` is the only one that is not an abort.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Completed,
    /// A target refused with `reason=mode`.
    ModeLeft,
    /// A target refused otherwise, or answered without the marker.
    Unverified,
    /// No verified target for `startTimeoutMs`.
    NoTargets,
    /// The controller said `stop`, or the run is gone.
    Stopped,
    /// No answer of the load API for three intervals.
    ControlLost,
    Terminated,
}

impl Reason {
    /// The `reason` of the final post.
    pub fn post(self) -> Option<&'static str> {
        match self {
            Reason::Completed => Some("Completed"),
            Reason::ModeLeft => Some("ModeLeft"),
            Reason::Unverified => Some("Unverified"),
            Reason::NoTargets => Some("NoTargets"),
            Reason::Stopped | Reason::ControlLost | Reason::Terminated => None,
        }
    }

    pub fn exit_code(self) -> u8 {
        u8::from(self != Reason::Completed)
    }
}

struct Target {
    ip: IpAddr,
    pod: String,
    /// Not skipped since the last targets poll.
    usable: bool,
}

/// What a send attempt came to.
enum Flow {
    Sent,
    /// No usable target: nothing was sent.
    Idle,
}

enum Acquired {
    Conn(Conn, bool),
    Skipped,
    Failed(ErrorKind),
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn millis(since: Instant) -> u64 {
    u64::try_from(since.elapsed().as_millis()).unwrap_or(u64::MAX)
}

fn class_label(status: u16) -> &'static str {
    match status {
        200..=299 => "2xx",
        300..=399 => "3xx",
        400..=499 => "4xx",
        500..=599 => "5xx",
        _ => "protocol",
    }
}

fn kind_label(kind: ErrorKind) -> &'static str {
    match kind {
        ErrorKind::Timeout => "timeout",
        ErrorKind::Connect => "connect",
        ErrorKind::Reset => "reset",
        ErrorKind::Protocol => "protocol",
        ErrorKind::Refused => "refused",
        ErrorKind::Unverified => "unverified",
    }
}

/// A request in flight, counted until it is dropped.
struct Slot(Arc<Shared>);

impl Slot {
    fn new(shared: &Arc<Shared>) -> Slot {
        shared.in_flight.fetch_add(1, Ordering::SeqCst);
        shared.metrics.in_flight.inc();
        Slot(shared.clone())
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        self.0.in_flight.fetch_sub(1, Ordering::SeqCst);
        self.0.metrics.in_flight.dec();
    }
}

struct Shared {
    config: Arc<Config>,
    plan: Plan,
    ticket: String,
    secrets: BTreeMap<String, String>,
    params: PodParams,
    /// `<METHOD> <template>` of each plan endpoint.
    labels: Vec<String>,
    /// Running sums of the endpoints' weights, for the draw of a missed send.
    cumulative: Vec<u64>,
    metrics: LoadMetrics,
    targets: Mutex<Vec<Target>>,
    results: Mutex<PodResults>,
    stop: watch::Sender<Option<Reason>>,
    pool: Mutex<BTreeMap<(IpAddr, u16), Vec<Conn>>>,
    /// The last time a probe passed or a response carried the marker.
    verified_at: Mutex<Instant>,
    in_flight: AtomicU64,
    unbuilt_logged: AtomicBool,
}

impl Shared {
    fn halt(&self, reason: Reason) {
        self.stop.send_if_modified(|current| {
            if current.is_some() {
                return false;
            }
            *current = Some(reason);
            true
        });
    }

    fn reason(&self) -> Option<Reason> {
        *self.stop.borrow()
    }

    async fn stopped(&self) {
        let mut rx = self.stop.subscribe();
        let _ = rx.wait_for(Option::is_some).await;
    }

    fn touch_verified(&self) {
        *lock(&self.verified_at) = Instant::now();
    }

    fn set_targets(&self, list: Vec<(IpAddr, String)>) {
        let mut targets = lock(&self.targets);
        *targets = list
            .into_iter()
            .map(|(ip, pod)| Target {
                ip,
                pod,
                usable: true,
            })
            .collect();
        self.metrics.targets.set(targets.len() as i64);
    }

    fn pick_target(&self) -> Option<(IpAddr, String)> {
        let targets = lock(&self.targets);
        let usable: Vec<&Target> = targets.iter().filter(|t| t.usable).collect();
        if usable.is_empty() {
            return None;
        }
        let pick = usable[rand::random_range(0..usable.len())];
        Some((pick.ip, pick.pod.clone()))
    }

    fn has_usable(&self) -> bool {
        lock(&self.targets).iter().any(|t| t.usable)
    }

    /// Skips `pod` until the next targets poll; the first time, one WARN.
    fn unverified(&self, pod: &str, why: crate::http::Unverified) {
        let mut targets = lock(&self.targets);
        let first = targets.iter_mut().any(|t| {
            let first = t.pod == pod && t.usable;
            if t.pod == pod {
                t.usable = false;
            }
            first
        });
        self.metrics
            .targets
            .set(targets.iter().filter(|t| t.usable).count() as i64);
        drop(targets);
        if first {
            tracing::warn!(pod, reason = why.as_str(), "target not verified");
        }
    }

    /// Probes every target of the list: those that fail are skipped.
    async fn verify_targets(self: &Arc<Self>) {
        let list: Vec<(IpAddr, String)> = lock(&self.targets)
            .iter()
            .map(|t| (t.ip, t.pod.clone()))
            .collect();
        let mut probes = JoinSet::new();
        for (ip, pod) in list {
            let shared = self.clone();
            probes.spawn(async move {
                let result = probe(
                    ip,
                    shared.config.probe_port,
                    &shared.config.run,
                    &shared.config.deployment,
                    &pod,
                )
                .await;
                (pod, result)
            });
        }
        while let Some(Ok((pod, result))) = probes.join_next().await {
            match result {
                Ok(()) => self.touch_verified(),
                Err(why) => self.unverified(&pod, why),
            }
        }
    }

    fn pooled(&self, ip: IpAddr, port: u16) -> Option<Conn> {
        let mut pool = lock(&self.pool);
        let idle = pool.get_mut(&(ip, port))?;
        while let Some(conn) = idle.pop() {
            if conn.alive() {
                return Some(conn);
            }
        }
        None
    }

    fn release(&self, ip: IpAddr, port: u16, conn: Conn) {
        let mut pool = lock(&self.pool);
        // Idle connections are bounded per target and together.
        let total: usize = pool.values().map(Vec::len).sum();
        let idle = pool.entry((ip, port)).or_default();
        if (idle.len() as u64) < self.params.concurrency && total < MAX_IDLE {
            idle.push(conn);
        }
    }

    /// A connection to `ip:port`: an idle one, or a new one after the probe of the target.
    async fn acquire(&self, ip: IpAddr, pod: &str, port: u16, timeout: Duration) -> Acquired {
        if let Some(conn) = self.pooled(ip, port) {
            return Acquired::Conn(conn, true);
        }
        match probe(
            ip,
            self.config.probe_port,
            &self.config.run,
            &self.config.deployment,
            pod,
        )
        .await
        {
            Ok(()) => self.touch_verified(),
            Err(why) => {
                self.unverified(pod, why);
                return Acquired::Skipped;
            }
        }
        match Conn::connect(SocketAddr::new(ip, port), timeout).await {
            Ok(conn) => Acquired::Conn(conn, false),
            Err(kind) => Acquired::Failed(kind),
        }
    }

    fn record_error(&self, endpoint: usize, kind: ErrorKind) {
        if let Some(stats) = lock(&self.results).endpoints.get_mut(endpoint) {
            stats.error(kind);
        }
        self.metrics
            .result(&self.labels[endpoint], kind_label(kind));
    }

    fn record_response(&self, endpoint: usize, status: u16, micros: u64, marker: Marker) {
        match marker {
            Marker::Ok => {
                if let Some(stats) = lock(&self.results).endpoints.get_mut(endpoint) {
                    stats.response(status, micros);
                }
                self.metrics
                    .result(&self.labels[endpoint], class_label(status));
                self.metrics.duration(&self.labels[endpoint], micros);
                self.touch_verified();
            }
            Marker::Refused(reason) if reason == "mode" => {
                self.record_error(endpoint, ErrorKind::Refused);
                self.halt(Reason::ModeLeft);
            }
            // Another deployment or pod, a bad ticket, or a pod that is not a mimic sidecar.
            Marker::Refused(_) | Marker::Missing => {
                self.record_error(endpoint, ErrorKind::Unverified);
                self.halt(Reason::Unverified);
            }
        }
    }

    fn record_missed(&self, count: u64) {
        let total = self.cumulative.last().copied().unwrap_or(0);
        let mut results = lock(&self.results);
        for _ in 0..count {
            let draw = if total == 0 {
                0
            } else {
                rand::random_range(0..total)
            };
            let index = self.cumulative.partition_point(|sum| *sum <= draw);
            if let Some(stats) = results.endpoints.get_mut(index) {
                stats.missed(1);
            }
        }
        drop(results);
        self.metrics.missed.inc_by(count);
    }

    fn unbuilt(&self, error: &RequestError) {
        if !self.unbuilt_logged.swap(true, Ordering::SeqCst) {
            let kind = match error {
                RequestError::Empty => "empty plan",
                RequestError::MissingSecret(_) => "missing secret key",
                RequestError::Invalid(what) => what,
                RequestError::Redacted => "redacted value",
            };
            tracing::warn!(kind, "request not built");
        }
    }

    /// One request: a target, a request from the plan, the exchange.
    async fn send_once(self: &Arc<Self>) -> Flow {
        let Some((ip, pod)) = self.pick_target() else {
            return Flow::Idle;
        };
        let seed = rand::random::<u64>();
        let built = {
            let context = Context {
                marker: &self.ticket,
                ip,
                now_ms: unix_ms(),
                secrets: &self.secrets,
            };
            request(&self.plan, &context, seed)
        };
        let request = match built {
            Ok(request) => request,
            Err(error) => {
                self.unbuilt(&error);
                sleep(Duration::from_millis(1)).await;
                return Flow::Sent;
            }
        };
        let endpoint = request.endpoint;
        let port = self.plan.endpoints[endpoint].port;
        let Some(prepared) = Prepared::new(request) else {
            self.unbuilt(&RequestError::Invalid("request"));
            return Flow::Sent;
        };
        let timeout = Duration::from_millis(self.params.timeout_ms);
        let mut retried = false;
        loop {
            let (mut conn, pooled) = match self.acquire(ip, &pod, port, timeout).await {
                Acquired::Conn(conn, pooled) => (conn, pooled),
                Acquired::Skipped => return Flow::Idle,
                Acquired::Failed(kind) => {
                    self.record_error(endpoint, kind);
                    return Flow::Sent;
                }
            };
            let outcome = tokio::select! {
                outcome = conn.send(&prepared, timeout) => outcome,
                () = self.stopped() => return Flow::Sent,
            };
            match outcome {
                // An idle connection the target closed meanwhile: once on a new one.
                Outcome::Error(ErrorKind::Reset) if pooled && !retried => retried = true,
                Outcome::Error(kind) => {
                    self.record_error(endpoint, kind);
                    return Flow::Sent;
                }
                Outcome::Response {
                    status,
                    micros,
                    marker,
                    reusable,
                } => {
                    let accepted = marker == Marker::Ok;
                    self.record_response(endpoint, status, micros, marker);
                    if reusable && accepted {
                        self.release(ip, port, conn);
                    }
                    return Flow::Sent;
                }
            }
        }
    }

    async fn closed_worker(self: Arc<Self>, index: u64, begun: Instant) {
        let p = &self.params;
        loop {
            if self.reason().is_some() {
                return;
            }
            let elapsed = millis(begun);
            if elapsed >= p.duration_ms {
                return;
            }
            if index >= workers(p.concurrency, p.ramp_up_ms, p.duration_ms, elapsed) {
                sleep(Duration::from_millis(20)).await;
                continue;
            }
            let slot = Slot::new(&self);
            let flow = self.send_once().await;
            drop(slot);
            if matches!(flow, Flow::Idle) {
                sleep(Duration::from_millis(50)).await;
            }
        }
    }

    async fn open_pacer(self: Arc<Self>, rate: u64, begun: Instant) {
        let p = self.params.clone();
        let mut pacer = Pacer::new(rate, p.ramp_up_ms, p.duration_ms, p.concurrency);
        let mut sends = JoinSet::new();
        let mut tick = tokio::time::interval(Duration::from_millis(5));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);
        loop {
            tokio::select! {
                _ = tick.tick() => {}
                () = self.stopped() => break,
            }
            let elapsed = millis(begun);
            while sends.try_join_next().is_some() {}
            let due = pacer.tick(elapsed, self.in_flight.load(Ordering::SeqCst));
            let mut missed = due.missed;
            for _ in 0..due.send {
                let slot = Slot::new(&self);
                let shared = self.clone();
                sends.spawn(async move {
                    let flow = shared.send_once().await;
                    drop(slot);
                    if matches!(flow, Flow::Idle) {
                        // No verified target: the send was due and not made.
                        shared.record_missed(1);
                    }
                });
            }
            if missed > 0 {
                missed = missed.min(1 << 20);
                self.record_missed(missed);
            }
            if elapsed >= p.duration_ms {
                break;
            }
        }
        while sends.join_next().await.is_some() {}
    }

    /// Starts the load; the task ends when every request is over.
    fn start(self: &Arc<Self>, begun: Instant) -> JoinHandle<()> {
        let shared = self.clone();
        tokio::spawn(async move {
            let mut tasks = JoinSet::new();
            match shared.params.rate {
                None => {
                    for index in 0..shared.params.concurrency {
                        tasks.spawn(shared.clone().closed_worker(index, begun));
                    }
                }
                Some(rate) => {
                    tasks.spawn(shared.clone().open_pacer(rate, begun));
                }
            }
            while tasks.join_next().await.is_some() {}
        })
    }

    fn snapshot(&self) -> PodResults {
        lock(&self.results).clone()
    }
}

/// Reads the credentials of the plan from the mounted Secret: one file per key.
fn read_secrets(config: &Config, plan: &Plan) -> Result<BTreeMap<String, String>, String> {
    let mut secrets = BTreeMap::new();
    for credential in &plan.credentials {
        if secrets.contains_key(&credential.key) {
            continue;
        }
        let key = credential.key.as_str();
        if key.is_empty() || key.contains('/') || key.starts_with("..") {
            return Err(format!("credential key {key:?} is not a file name"));
        }
        let Some(dir) = &config.credentials_dir else {
            return Err("the plan has credentials and MIMIC_LOAD_CREDENTIALS_DIR is unset".into());
        };
        let value = std::fs::read_to_string(dir.join(key))
            .map_err(|e| format!("credential key {key:?} unreadable: {}", e.kind()))?;
        if value.len() > 64 << 10 {
            return Err(format!("credential key {key:?} is over 64 KiB"));
        }
        secrets.insert(key.to_string(), value);
    }
    Ok(secrets)
}

fn check_params(p: &PodParams) -> Result<(), &'static str> {
    if !(1..=4096).contains(&p.concurrency) {
        return Err("params.concurrency out of range");
    }
    if p.rate == Some(0) {
        return Err("params.rate is 0");
    }
    if p.duration_ms == 0 || p.timeout_ms == 0 {
        return Err("params.durationMs and params.timeoutMs must be positive");
    }
    Ok(())
}

/// Waits for `duration`, or returns `false` at the signal.
async fn pause(duration: Duration, shutdown: &Shutdown) -> bool {
    tokio::select! {
        () = sleep(duration) => true,
        () = shutdown.clone().wait() => false,
    }
}

/// The plan, retried for three intervals.
/// Why there is no plan to run.
enum PlanError {
    /// Read, but not to be run: the generator posts and exits.
    Refused(String),
    Missing(String),
}

async fn fetch_plan(
    api: &Api,
    interval: Duration,
    shutdown: &Shutdown,
) -> Result<crate::api::PlanAnswer, PlanError> {
    let missing = |why: &str| PlanError::Missing(why.to_string());
    let begun = Instant::now();
    loop {
        match api.plan().await {
            Ok(answer) => return Ok(answer),
            Err(ApiError::Gone) => return Err(missing("the run is unknown or ended")),
            // A plan over the limit never gets smaller: refuse it, with a final post.
            Err(ApiError::TooLarge) => {
                return Err(PlanError::Refused("plan over MIMIC_LOAD_MAX_PLAN".into()));
            }
            Err(error) => {
                tracing::warn!(error = %error, "plan not read");
                if begun.elapsed() >= interval * CONTROL_INTERVALS {
                    return Err(missing("no plan for three intervals"));
                }
                if !pause(interval, shutdown).await {
                    return Err(missing("stopped before the plan was read"));
                }
            }
        }
    }
}

/// Posts the final results; a few tries, within the grace period.
async fn post_final(api: &Api, shared: &Shared, reason: Reason, grace: Duration) {
    let results = shared.snapshot();
    let post = Post {
        final_post: true,
        pod: &shared.config.pod,
        reason: reason.post(),
        results: &results,
    };
    let tries = if reason == Reason::ControlLost { 1 } else { 3 };
    let attempts = async {
        for attempt in 0..tries {
            match api.post(&post).await {
                Ok(_) => return true,
                Err(error) => {
                    tracing::warn!(error = %error, "final results not posted");
                    if attempt + 1 < tries {
                        sleep(Duration::from_millis(500)).await;
                    }
                }
            }
        }
        false
    };
    if tokio::time::timeout(grace, attempts).await.is_err() {
        tracing::warn!("final results not posted within the grace period");
    }
}

/// Idle connections kept in all, over every target.
const MAX_IDLE: usize = 512;

/// A plan or parameters the generator will not run: no request, a final post, exit code 1.
async fn refuse(api: &Api, config: &Config, why: String) -> u8 {
    tracing::error!(error = %why, "plan refused");
    let empty = PodResults::default();
    let post = Post {
        final_post: true,
        pod: &config.pod,
        reason: None,
        results: &empty,
    };
    let _ = tokio::time::timeout(config.common.shutdown_grace, api.post(&post)).await;
    1
}

/// Runs the generator to its end and returns the process exit code.
pub async fn generate(
    config: Arc<Config>,
    metrics: LoadMetrics,
    ready: Arc<AtomicBool>,
    shutdown: Shutdown,
) -> u8 {
    let begun = Instant::now();
    let interval = config.interval;
    let grace = config.common.shutdown_grace;
    let api = match Api::new(&config) {
        Ok(api) => api,
        Err(error) => {
            tracing::error!(error = %error, "cannot build the load API client");
            return 1;
        }
    };
    let answer = match fetch_plan(&api, interval, &shutdown).await {
        Ok(answer) => answer,
        Err(PlanError::Refused(why)) => return refuse(&api, &config, why).await,
        Err(PlanError::Missing(error)) => {
            tracing::error!(error = %error, "no plan");
            return 1;
        }
    };
    let (ticket, params) = (answer.ticket, answer.params);
    let plan = match Plan::from_json(answer.plan.get(), config.max_plan) {
        Ok(plan) => plan,
        Err(why) => return refuse(&api, &config, why).await,
    };
    drop(answer.plan);
    if let Err(why) = check_params(&params) {
        return refuse(&api, &config, why.to_string()).await;
    }
    let secrets = match read_secrets(&config, &plan) {
        Ok(secrets) => secrets,
        Err(why) => return refuse(&api, &config, why).await,
    };

    let labels = plan
        .endpoints
        .iter()
        .map(|e| format!("{} {}", e.method, e.template))
        .collect();
    let mut sum = 0u64;
    let cumulative = plan
        .endpoints
        .iter()
        .map(|e| {
            sum = sum.saturating_add(e.weight);
            sum
        })
        .collect();
    let results = PodResults {
        endpoints: vec![Stats::default(); plan.endpoints.len()],
    };
    let shared = Arc::new(Shared {
        config: config.clone(),
        plan,
        ticket,
        secrets,
        params,
        labels,
        cumulative,
        metrics,
        targets: Mutex::new(Vec::new()),
        results: Mutex::new(results),
        stop: watch::channel(None).0,
        pool: Mutex::new(BTreeMap::new()),
        verified_at: Mutex::new(Instant::now()),
        in_flight: AtomicU64::new(0),
        unbuilt_logged: AtomicBool::new(false),
    });
    ready.store(true, Ordering::SeqCst);

    let reason = supervise(&shared, &api, &shutdown, begun).await;
    tracing::info!(reason = reason.post().unwrap_or("Aborted"), "load ended");
    post_final(&api, &shared, reason, grace).await;
    reason.exit_code()
}

/// Waits for a verified target, runs the load and reports every interval until it ends.
async fn supervise(shared: &Arc<Shared>, api: &Api, shutdown: &Shutdown, begun: Instant) -> Reason {
    let interval = shared.config.interval;
    let start_timeout = Duration::from_millis(shared.params.start_timeout_ms);
    let mut control = Instant::now();

    // A verified target before the clock of the run starts.
    loop {
        match api.targets().await {
            Ok(answer) => {
                control = Instant::now();
                if answer.stop {
                    return Reason::Stopped;
                }
                shared.set_targets(answer.addresses());
            }
            Err(ApiError::Gone) => return Reason::Stopped,
            Err(error) => tracing::warn!(error = %error, "targets not read"),
        }
        shared.verify_targets().await;
        if shared.has_usable() {
            break;
        }
        if begun.elapsed() >= start_timeout {
            return Reason::NoTargets;
        }
        if control.elapsed() >= interval * CONTROL_INTERVALS {
            return Reason::ControlLost;
        }
        if !pause(interval, shutdown).await {
            return Reason::Terminated;
        }
    }

    tracing::info!("load started");
    shared.touch_verified();
    let run_begun = Instant::now();
    let mut load = shared.start(run_begun);
    let mut tick = tokio::time::interval(interval);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    tick.tick().await;
    // The poll and the post run beside the loop: a slow controller never delays a stop.
    // The last answer came before the probes, which can take 2 s: the run starts a new count.
    let _ = control;
    let control = Arc::new(Mutex::new(Instant::now()));
    let mut reporting: Option<tokio::task::JoinHandle<()>> = None;
    loop {
        tokio::select! {
            _ = tick.tick() => {
                if reporting.as_ref().is_none_or(tokio::task::JoinHandle::is_finished) {
                    let (shared, api, control) = (shared.clone(), api.clone(), control.clone());
                    reporting = Some(tokio::spawn(async move {
                        report(&shared, &api, &control).await;
                    }));
                }
                if lock(&control).elapsed() >= interval * CONTROL_INTERVALS {
                    shared.halt(Reason::ControlLost);
                }
                if lock(&shared.verified_at).elapsed() >= start_timeout {
                    shared.halt(Reason::NoTargets);
                }
            }
            () = shared.stopped() => {}
            _ = &mut load => {
                shared.halt(Reason::Completed);
            }
            () = shutdown.clone().wait() => shared.halt(Reason::Terminated),
        }
        if shared.reason().is_some() {
            break;
        }
    }
    if let Some(handle) = reporting {
        handle.abort();
    }
    let reason = shared.reason().unwrap_or(Reason::Completed);
    if !load.is_finished() && tokio::time::timeout(WIND_DOWN, &mut load).await.is_err() {
        load.abort();
    }
    reason
}

/// One poll of the targets and one post of the results; `control` is the last answer.
async fn report(shared: &Arc<Shared>, api: &Api, control: &Mutex<Instant>) {
    match api.targets().await {
        Ok(answer) => {
            *lock(control) = Instant::now();
            if answer.stop {
                shared.halt(Reason::Stopped);
            } else {
                shared.set_targets(answer.addresses());
            }
        }
        Err(ApiError::Gone) => shared.halt(Reason::Stopped),
        Err(error) => tracing::warn!(error = %error, "targets not read"),
    }
    let results = shared.snapshot();
    let post = Post {
        final_post: false,
        pod: &shared.config.pod,
        reason: None,
        results: &results,
    };
    match api.post(&post).await {
        Ok(stop) => {
            *lock(control) = Instant::now();
            if stop {
                shared.halt(Reason::Stopped);
            }
        }
        Err(ApiError::Gone) => shared.halt(Reason::Stopped),
        Err(error) => tracing::warn!(error = %error, "results not posted"),
    }
}
