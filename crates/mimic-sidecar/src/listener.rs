//! Listeners of the sidecar (spec §4, §10.3): configuration, binding, the accept loop and the
//! counters of the connection layer. Each accepted connection goes to [`crate::connection`].

use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use mimic_runtime::config::{ConfigError, Env};
use mimic_runtime::metrics::Metrics;
use nix::sys::resource::{Resource, getrlimit, setrlimit};
use nix::sys::socket::{getsockopt, sockopt};
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use socket2::{Domain, Socket, Type};
use tokio::net::{TcpListener, TcpStream};

use crate::connection;

const ACCEPT_RETRY: Duration = Duration::from_millis(100);
const ACCEPT_LOG_INTERVAL: Duration = Duration::from_secs(1);
const IPV6_INTERFACES: &str = "/proc/net/if_inet6";

const INBOUND_VAR: &str = "MIMIC_INBOUND_ADDRS";
const OUTBOUND_VAR: &str = "MIMIC_OUTBOUND_ADDRS";

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
    original_dst: OriginalDst,
}

impl Shared {
    /// The destination `stream` was addressed to before the redirection.
    pub(crate) fn original_dst(&self, stream: &TcpStream) -> io::Result<SocketAddr> {
        (self.original_dst)(stream)
    }

    pub(crate) fn metrics(&self) -> &TunnelMetrics {
        &self.metrics
    }

    pub fn new(
        bound: Vec<SocketAddr>,
        metrics: TunnelMetrics,
        original_dst: OriginalDst,
    ) -> Shared {
        Shared {
            bound,
            metrics,
            original_dst,
        }
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

/// Listener addresses (spec §10.3).
#[derive(Debug, Clone)]
pub struct TunnelConfig {
    pub inbound: Vec<SocketAddr>,
    pub outbound: Vec<SocketAddr>,
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
        Ok(TunnelConfig { inbound, outbound })
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
    socket.listen(1024)?;
    TcpListener::from_std(socket.into())
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

/// Raises the open-file soft limit to the hard limit (spec §4.1): the proxy holds two sockets
/// per connection and never refuses one. Failures are logged, not fatal.
pub fn raise_fd_limit() {
    match getrlimit(Resource::RLIMIT_NOFILE) {
        Ok((soft, hard)) if soft < hard => match setrlimit(Resource::RLIMIT_NOFILE, hard, hard) {
            Ok(()) => tracing::info!(from = soft, to = hard, "open-file limit raised"),
            Err(e) => tracing::warn!(error = %e, soft, hard, "cannot raise the open-file limit"),
        },
        Ok(_) => {}
        Err(e) => tracing::warn!(error = %e, "cannot read the open-file limit"),
    }
}

/// Accepts connections on `listener` for as long as the runtime lives.
pub fn spawn_listener(listener: TcpListener, direction: Direction, shared: Arc<Shared>) {
    tokio::spawn(accept_loop(
        listener,
        direction,
        shared,
        std::future::pending(),
    ));
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
        let accepted = tokio::select! {
            () = &mut shutdown => return,
            accepted = listener.accept() => accepted,
        };
        match accepted {
            Ok((stream, _)) => {
                tokio::spawn(connection::handle(stream, direction, shared.clone()));
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
