//! Listeners of the sidecar (spec §4, §10.3): configuration, binding, the accept loop and the
//! counters of the connection layer. Each accepted connection goes to [`crate::connection`].

use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mimic_core::fd::fd_limit_target;
use mimic_runtime::config::{ConfigError, Env};
use mimic_runtime::metrics::Metrics;
use nix::sys::resource::{Resource, getrlimit, setrlimit};
use nix::sys::socket::{getsockopt, sockopt};
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use socket2::{Domain, Socket, Type};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

use crate::connection;
use crate::red::{DEFAULT_MAX_AUTHORITIES, ProxyMetrics};
use crate::tls::intercept::Interception;

const ACCEPT_RETRY: Duration = Duration::from_millis(100);
/// Time given to the connections to finish closing once they were told to.
const ABORT_WAIT: Duration = Duration::from_secs(1);
const ACCEPT_LOG_INTERVAL: Duration = Duration::from_secs(1);
const IPV6_INTERFACES: &str = "/proc/net/if_inet6";

const INBOUND_VAR: &str = "MIMIC_INBOUND_ADDRS";
const OUTBOUND_VAR: &str = "MIMIC_OUTBOUND_ADDRS";
const MAX_AUTHORITIES_VAR: &str = "MIMIC_METRICS_MAX_AUTHORITIES";
const MAX_AUTHORITIES_RANGE: std::ops::RangeInclusive<usize> = 1..=10_000;
const MAX_CONNECTIONS_VAR: &str = "MIMIC_MAX_CONNECTIONS";
const MAX_CONNECTIONS_RANGE: std::ops::RangeInclusive<usize> = 1..=1_000_000;
/// Connections served at once unless `MIMIC_MAX_CONNECTIONS` says otherwise.
pub const DEFAULT_MAX_CONNECTIONS: usize = 16_384;
/// A connection with an incomplete protocol prefix that stays silent this long is reset.
pub const DETECT_STALL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Direction {
    Inbound,
    Outbound,
}

impl Direction {
    pub fn as_str(self) -> &'static str {
        match self {
            Direction::Inbound => "inbound",
            Direction::Outbound => "outbound",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum Outcome {
    Ok,
    ConnectError,
    Loop,
    NoOriginalDst,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::ConnectError => "connect_error",
            Outcome::Loop => "loop",
            Outcome::NoOriginalDst => "no_original_dst",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct Labels {
    direction: &'static str,
    result: &'static str,
}

/// `mimic_tcp_connections_total{direction,result}` and `mimic_accept_errors_total{direction}`.
#[derive(Clone, Default)]
pub struct TunnelMetrics {
    connections: Family<Labels, Counter>,
    accept_errors: Family<DirectionLabel, Counter>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct DirectionLabel {
    direction: &'static str,
}

impl TunnelMetrics {
    pub fn register(metrics: &mut Metrics) -> TunnelMetrics {
        let tunnel = TunnelMetrics::default();
        metrics.registry_mut().register(
            "tcp_connections",
            "Connections handled by the TCP tunnel",
            tunnel.connections.clone(),
        );
        metrics.registry_mut().register(
            "accept_errors",
            "Failed accept calls on the sidecar listeners",
            tunnel.accept_errors.clone(),
        );
        tunnel
    }

    pub(crate) fn count(&self, direction: Direction, result: Outcome) {
        self.connections
            .get_or_create(&Labels {
                direction: direction.as_str(),
                result: result.as_str(),
            })
            .inc();
    }
}

/// Returns the destination a redirected connection was addressed to.
pub type OriginalDst = Arc<dyn Fn(&TcpStream) -> io::Result<SocketAddr> + Send + Sync>;

/// State shared by every listener and connection.
pub struct Shared {
    bound: Vec<SocketAddr>,
    metrics: TunnelMetrics,
    proxy: ProxyMetrics,
    tls: Interception,
    discovery: Option<crate::discovery::Tap>,
    original_dst: OriginalDst,
    stop: watch::Sender<bool>,
    abort: watch::Sender<bool>,
    active: watch::Sender<usize>,
    /// One permit per connection served at once, per direction (inbound, outbound).
    permits: [Arc<Semaphore>; 2],
    detect_stall: Duration,
}

/// Counts a connection as active until dropped, and holds its slot under the cap.
pub(crate) struct Active(
    watch::Sender<usize>,
    #[allow(dead_code)] OwnedSemaphorePermit,
);

impl Drop for Active {
    fn drop(&mut self) {
        self.0.send_modify(|n| *n -= 1);
    }
}

impl Shared {
    /// The destination `stream` was addressed to before the redirection.
    pub(crate) fn original_dst(&self, stream: &TcpStream) -> io::Result<SocketAddr> {
        (self.original_dst)(stream)
    }

    pub(crate) fn metrics(&self) -> &TunnelMetrics {
        &self.metrics
    }

    pub(crate) fn proxy(&self) -> &ProxyMetrics {
        &self.proxy
    }

    pub(crate) fn tls(&self) -> &Interception {
        &self.tls
    }

    pub fn new(
        bound: Vec<SocketAddr>,
        metrics: TunnelMetrics,
        original_dst: OriginalDst,
    ) -> Shared {
        Shared {
            bound,
            metrics,
            proxy: ProxyMetrics::default(),
            tls: Interception::default(),
            discovery: None,
            original_dst,
            stop: watch::channel(false).0,
            abort: watch::channel(false).0,
            active: watch::channel(0).0,
            permits: [
                Arc::new(Semaphore::new(DEFAULT_MAX_CONNECTIONS)),
                Arc::new(Semaphore::new(DEFAULT_MAX_CONNECTIONS)),
            ],
            detect_stall: DETECT_STALL,
        }
    }

    /// Starts the shutdown: the listeners stop accepting and the relays wind down.
    pub fn shutdown(&self) {
        self.stop.send_replace(true);
    }

    /// Resolves once [`Shared::shutdown`] was called, also if it was before this call.
    pub fn stopped(&self) -> impl Future<Output = ()> + Send + 'static {
        let mut stop = self.stop.subscribe();
        async move {
            let _ = stop.wait_for(|stopped| *stopped).await;
        }
    }

    /// Resolves once the grace ended and the connections left must close ([`Shared::drain`]).
    pub(crate) fn aborted(&self) -> impl Future<Output = ()> + Send + 'static {
        let mut abort = self.abort.subscribe();
        async move {
            let _ = abort.wait_for(|aborted| *aborted).await;
        }
    }

    /// The abort flag, for the exchanges that must tell a closure at the end of the grace.
    pub(crate) fn abort_flag(&self) -> watch::Receiver<bool> {
        self.abort.subscribe()
    }

    /// Waits at most `grace` for the connections to end by themselves. Then closes the ones
    /// left (their access line says `shutdown`), logs how many, and returns that number.
    pub async fn drain(&self, grace: Duration) -> usize {
        if tokio::time::timeout(grace, self.drained()).await.is_ok() {
            return 0;
        }
        let connections = *self.active.borrow();
        self.abort.send_replace(true);
        tracing::warn!(connections, "grace period over");
        // Closing is immediate; this only lets the access lines out.
        let _ = tokio::time::timeout(ABORT_WAIT, self.drained()).await;
        connections
    }

    /// Resolves when no accepted connection is left.
    pub async fn drained(&self) {
        let _ = self.active.subscribe().wait_for(|n| *n == 0).await;
    }

    fn track(&self, slot: OwnedSemaphorePermit) -> Active {
        self.active.send_modify(|n| *n += 1);
        Active(self.active.clone(), slot)
    }

    /// Uses `proxy` for the RED metrics instead of unregistered ones.
    pub fn with_proxy_metrics(mut self, proxy: ProxyMetrics) -> Shared {
        self.proxy = proxy;
        self
    }

    /// Serves at most `max` connections at once in each direction (spec §4.1).
    pub fn with_max_connections(mut self, max: usize) -> Shared {
        self.permits = [Arc::new(Semaphore::new(max)), Arc::new(Semaphore::new(max))];
        self
    }

    fn permits(&self, direction: Direction) -> Arc<Semaphore> {
        self.permits[usize::from(direction == Direction::Outbound)].clone()
    }

    /// Resets a connection stalled on an incomplete protocol prefix after `stall` (spec §4.1).
    pub fn with_detect_stall(mut self, stall: Duration) -> Shared {
        self.detect_stall = stall;
        self
    }

    pub(crate) fn detect_stall(&self) -> Duration {
        self.detect_stall
    }

    /// Hands the inbound HTTP/1 exchanges to discovery (spec §6).
    pub fn with_discovery(mut self, tap: crate::discovery::Tap) -> Shared {
        self.discovery = Some(tap);
        self
    }

    pub(crate) fn discovery(&self) -> Option<&crate::discovery::Tap> {
        self.discovery.as_ref()
    }

    /// Intercepts outbound TLS as `tls` says instead of passing it all through.
    pub fn with_tls(mut self, tls: Interception) -> Shared {
        self.tls = tls;
        self
    }

    /// True when `dst` is one of the sidecar's own listeners: a connection to it would loop.
    /// An unspecified listener address matches any address on its port.
    pub fn is_listener(&self, dst: SocketAddr) -> bool {
        self.bound
            .iter()
            .any(|b| b.port() == dst.port() && (b.ip().is_unspecified() || b.ip() == dst.ip()))
    }
}

/// The destination read from netfilter's `SO_ORIGINAL_DST` / `IP6T_SO_ORIGINAL_DST`; fails when
/// the connection was not redirected.
pub fn kernel_original_dst(stream: &TcpStream) -> io::Result<SocketAddr> {
    if stream.local_addr()?.is_ipv6() {
        let sa = getsockopt(stream, sockopt::Ip6tOriginalDst).map_err(io::Error::from)?;
        Ok(SocketAddr::new(
            IpAddr::V6(Ipv6Addr::from(sa.sin6_addr.s6_addr)),
            u16::from_be(sa.sin6_port),
        ))
    } else {
        let sa = getsockopt(stream, sockopt::OriginalDst).map_err(io::Error::from)?;
        Ok(SocketAddr::new(
            IpAddr::V4(Ipv4Addr::from(u32::from_be(sa.sin_addr.s_addr))),
            u16::from_be(sa.sin_port),
        ))
    }
}

/// Listener addresses and metric label budget (spec §10.3).
#[derive(Debug, Clone)]
pub struct TunnelConfig {
    pub inbound: Vec<SocketAddr>,
    pub outbound: Vec<SocketAddr>,
    /// Distinct `authority` label values kept per direction and metric.
    pub max_authorities: usize,
    /// Connections served at once (both directions).
    pub max_connections: usize,
}

impl TunnelConfig {
    pub fn from_env(env: &Env) -> Result<TunnelConfig, ConfigError> {
        let inbound = addrs(env, INBOUND_VAR, "0.0.0.0:15006,[::]:15006")?;
        let outbound = addrs(env, OUTBOUND_VAR, "127.0.0.1:15001,[::1]:15001")?;
        if let Some(a) = outbound.iter().find(|a| !a.ip().is_loopback()) {
            return Err(ConfigError::invalid(
                OUTBOUND_VAR,
                &a.to_string(),
                "outbound listeners accept loopback connections only",
            ));
        }
        let max_authorities = env.parse_or(MAX_AUTHORITIES_VAR, DEFAULT_MAX_AUTHORITIES)?;
        if !MAX_AUTHORITIES_RANGE.contains(&max_authorities) {
            return Err(ConfigError::invalid(
                MAX_AUTHORITIES_VAR,
                &max_authorities.to_string(),
                "expected a number from 1 to 10000",
            ));
        }
        let max_connections = env.parse_or(MAX_CONNECTIONS_VAR, DEFAULT_MAX_CONNECTIONS)?;
        if !MAX_CONNECTIONS_RANGE.contains(&max_connections) {
            return Err(ConfigError::invalid(
                MAX_CONNECTIONS_VAR,
                &max_connections.to_string(),
                "expected a number from 1 to 1000000",
            ));
        }
        Ok(TunnelConfig {
            inbound,
            outbound,
            max_authorities,
            max_connections,
        })
    }
}

fn addrs(env: &Env, var: &str, default: &str) -> Result<Vec<SocketAddr>, ConfigError> {
    let value = env.optional(var).unwrap_or(default);
    value
        .split(',')
        .map(|a| {
            a.trim()
                .parse::<SocketAddr>()
                .map_err(|e| ConfigError::invalid(var, value, e))
        })
        .collect()
}

/// True when the pod has an IPv6 address (spec §4).
pub fn pod_has_ipv6() -> bool {
    std::fs::read_to_string(IPV6_INTERFACES).is_ok_and(|s| !s.trim().is_empty())
}

/// Binds `addr`; an IPv6 socket is `V6ONLY` so that `0.0.0.0` and `[::]` can share a port.
pub fn bind(addr: SocketAddr) -> io::Result<TcpListener> {
    let socket = Socket::new(Domain::for_address(addr), Type::STREAM, None)?;
    if addr.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&addr.into())?;
    socket.listen(backlog())?;
    TcpListener::from_std(socket.into())
}

/// The accept queue length for the content of `/proc/sys/net/core/somaxconn`: its value when
/// it is a positive number, else 1024.
fn backlog_from(somaxconn: Option<&str>) -> i32 {
    somaxconn
        .and_then(|s| s.trim().parse::<u32>().ok())
        .filter(|n| *n > 0)
        .map_or(1024, |n| i32::try_from(n).unwrap_or(i32::MAX))
}

fn backlog() -> i32 {
    backlog_from(
        std::fs::read_to_string("/proc/sys/net/core/somaxconn")
            .ok()
            .as_deref(),
    )
}

/// Binds every address, skipping IPv6 ones when the pod has no IPv6. Returns the listeners
/// with the address each one is really bound to.
pub fn bind_all(
    direction: Direction,
    addrs: &[SocketAddr],
    ipv6: bool,
) -> io::Result<Vec<(TcpListener, SocketAddr)>> {
    let mut bound = Vec::new();
    for addr in addrs {
        if addr.is_ipv6() && !ipv6 {
            tracing::info!(direction = direction.as_str(), %addr, "pod has no IPv6, listener skipped");
            continue;
        }
        let listener = bind(*addr).map_err(|e| {
            io::Error::new(
                e.kind(),
                format!("bind {} listener {addr}: {e}", direction.as_str()),
            )
        })?;
        let local = listener.local_addr()?;
        tracing::info!(direction = direction.as_str(), addr = %local, "tunnel listening");
        bound.push((listener, local));
    }
    Ok(bound)
}

/// Raises the open-file soft limit to the hard limit, or to `fs.nr_open` when the hard limit
/// is unlimited (spec §4.1): the proxy holds two sockets per connection and never refuses one.
/// Failures are logged, not fatal.
pub fn raise_fd_limit() {
    match getrlimit(Resource::RLIMIT_NOFILE) {
        Ok((soft, hard)) => {
            let nr_open = std::fs::read_to_string("/proc/sys/fs/nr_open")
                .ok()
                .and_then(|v| v.trim().parse::<u64>().ok());
            let Some(target) = fd_limit_target(soft, hard, nr_open) else {
                return;
            };
            match setrlimit(Resource::RLIMIT_NOFILE, target, hard) {
                Ok(()) => tracing::info!(from = soft, to = target, "open-file limit raised"),
                Err(e) => {
                    tracing::warn!(error = %e, soft, hard, target, "cannot raise the open-file limit");
                }
            }
        }
        Err(e) => tracing::warn!(error = %e, "cannot read the open-file limit"),
    }
}

/// Accepts connections on `listener` until [`Shared::shutdown`].
pub fn spawn_listener(listener: TcpListener, direction: Direction, shared: Arc<Shared>) {
    let stopped = shared.stopped();
    tokio::spawn(accept_loop(listener, direction, shared, stopped));
}

/// Accepts connections on `listener` until `shutdown` completes; the connections already
/// accepted keep running. A failing `accept` (e.g. no descriptor left) is counted, logged at
/// most once per second and retried.
pub async fn accept_loop(
    listener: TcpListener,
    direction: Direction,
    shared: Arc<Shared>,
    shutdown: impl Future<Output = ()>,
) {
    tokio::pin!(shutdown);
    let mut last_logged: Option<Instant> = None;
    loop {
        // At the cap nothing is accepted: further connections wait in the backlog.
        let slot = tokio::select! {
            () = &mut shutdown => return,
            Ok(slot) = shared.permits(direction).acquire_owned() => slot,
        };
        let accepted = tokio::select! {
            () = &mut shutdown => return,
            accepted = listener.accept() => accepted,
        };
        match accepted {
            Ok((stream, _)) => {
                let active = shared.track(slot);
                tokio::spawn(connection::handle(
                    stream,
                    direction,
                    shared.clone(),
                    active,
                ));
            }
            Err(e) => {
                shared
                    .metrics
                    .accept_errors
                    .get_or_create(&DirectionLabel {
                        direction: direction.as_str(),
                    })
                    .inc();
                if last_logged.is_none_or(|t| t.elapsed() >= ACCEPT_LOG_INTERVAL) {
                    last_logged = Some(Instant::now());
                    tracing::warn!(direction = direction.as_str(), error = %e, "accept failed");
                }
                tokio::select! {
                    () = &mut shutdown => return,
                    () = tokio::time::sleep(ACCEPT_RETRY) => {}
                }
            }
        }
    }
}

#[cfg(test)]
mod backlog_tests {
    use super::backlog_from;

    #[test]
    fn dsc_16_backlog_follows_somaxconn_when_readable() {
        assert_eq!(backlog_from(Some("4096\n")), 4096);
        assert_eq!(backlog_from(Some("128")), 128);
        assert_eq!(backlog_from(Some("4294967295")), i32::MAX);
        for bad in [None, Some(""), Some("many"), Some("0"), Some("-1")] {
            assert_eq!(backlog_from(bad), 1024, "{bad:?}");
        }
    }
}
