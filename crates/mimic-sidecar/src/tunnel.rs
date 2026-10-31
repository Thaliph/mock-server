//! Opaque TCP tunnel (spec §4, INJ-07): accepts the connections redirected by `mimic-init`,
//! reads their original destination and relays the bytes. Protocol detection comes in F2.

use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use mimic_runtime::config::{ConfigError, Env};
use mimic_runtime::metrics::Metrics;
use nix::sys::socket::{getsockopt, sockopt};
use prometheus_client::encoding::EncodeLabelSet;
use prometheus_client::metrics::counter::Counter;
use prometheus_client::metrics::family::Family;
use socket2::{Domain, Socket, Type};
use tokio::io::copy_bidirectional;
use tokio::net::{TcpListener, TcpStream};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
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
enum Outcome {
    Ok,
    ConnectError,
    Loop,
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::ConnectError => "connect_error",
            Outcome::Loop => "loop",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, EncodeLabelSet)]
struct Labels {
    direction: &'static str,
    result: &'static str,
}

/// `mimic_tcp_connections_total{direction,result}`.
#[derive(Clone, Default)]
pub struct TunnelMetrics {
    connections: Family<Labels, Counter>,
}

impl TunnelMetrics {
    pub fn register(metrics: &mut Metrics) -> TunnelMetrics {
        let tunnel = TunnelMetrics::default();
        metrics.registry_mut().register(
            "tcp_connections",
            "Connections handled by the TCP tunnel",
            tunnel.connections.clone(),
        );
        tunnel
    }

    fn count(&self, direction: Direction, result: Outcome) {
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

/// Accepts connections on `listener` for as long as the runtime lives.
pub fn spawn_listener(listener: TcpListener, direction: Direction, shared: Arc<Shared>) {
    tokio::spawn(async move {
        loop {
            match listener.accept().await {
                Ok((stream, _)) => {
                    tokio::spawn(handle(stream, direction, shared.clone()));
                }
                Err(e) => {
                    tracing::warn!(direction = direction.as_str(), error = %e, "accept failed");
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    });
}

async fn handle(mut client: TcpStream, direction: Direction, shared: Arc<Shared>) {
    let dst = match (shared.original_dst)(&client) {
        Ok(dst) if !shared.is_listener(dst) => dst,
        found => {
            shared.metrics.count(direction, Outcome::Loop);
            tracing::debug!(direction = direction.as_str(), original_dst = ?found.ok(), "loop, connection closed");
            return;
        }
    };
    let connected = tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(dst)).await;
    let mut upstream = match connected {
        Ok(Ok(upstream)) => upstream,
        failure => {
            shared.metrics.count(direction, Outcome::ConnectError);
            let error = match failure {
                Ok(Err(e)) => e.to_string(),
                _ => "connect timeout".to_string(),
            };
            tracing::debug!(direction = direction.as_str(), %dst, %error, "connect failed");
            return;
        }
    };
    shared.metrics.count(direction, Outcome::Ok);
    if let Err(e) = copy_bidirectional(&mut client, &mut upstream).await {
        tracing::debug!(direction = direction.as_str(), %dst, error = %e, "tunnel ended");
    }
}
